//! The UI controller: the windows of the process, their tabs and the execution of commands.
//!
//! Window callbacks never run commands, dialogs or popup menus themselves (those pump messages
//! or touch the window host while the host is borrowed). They push [`Request`]s; the owner of
//! the message loop ([`super::UiShell`] or the test script runner) drains them with
//! [`Controller::take_request`] and calls [`Controller::run_command`] /
//! [`Controller::handle_action`] outside of any window procedure.

use std::cell::{Cell, Ref, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::command::CommandId;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::keymap::Keymap;
use ssw_core::settings::Settings;
use ssw_io::session::{Session, ViewState, WindowPlacement};
use ssw_ui::editor::blocks_media::MediaEvent;
use ssw_ui::editor::{
    DocumentKind, EditorEvent, LinkTarget, MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT, ZOOM_STEP_PERCENT,
};
use ssw_ui::geometry::Size;
use ssw_ui::input::Cursor;
use ssw_ui::services::{
    Clock, Dialogs, FileFilter, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions,
    MessageBoxResult, OpenFileOptions, PickFolderOptions, SaveFileOptions, Shell,
};
use ssw_ui::shell::banners::{BannerKind, HostEvent};
use ssw_ui::shell::layout::ShellMode;
use ssw_ui::shell::sidebar::SidebarEvent;
use ssw_ui::shell::tabs::TabsEvent;
use ssw_ui::shell::titlebar::{FormatToggle, TitleEvent};
use ssw_ui::shortcuts::{CharProbe, ShortcutOutcome, ShortcutRouter};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widgets::SharedClipboard;

use super::menus::MenuState;
use super::overlays::{OverlayAction, list_workspace_files, panel_files};
use super::shell_view::{ShellAction, ShellView, TabView, ViewConfig, sidebar_options};
use super::tree_source::{TreePaths, WorkspaceTree};
use crate::app::AppCore;
use crate::commands::{AppActions, Dispatch};
use crate::documents::{DocEntry, DocError, DocMode};
use crate::session::{PlannedDocument, SessionManager, StartupPlan, TabState, WindowState};
use crate::workers::{JobKey, JobKind, JobSpec, WindowId};

/// Placement of a window that has none stored.
pub const DEFAULT_PLACEMENT: WindowPlacement = WindowPlacement {
    x: 100,
    y: 100,
    width: 1280,
    height: 800,
    maximized: false,
};

/// Work a window callback hands to the owner of the message loop.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Run a command for a window (a shortcut or a menu item).
    Command(WindowId, CommandId),
    /// Carry out something the user did in the window's widgets.
    Action(WindowId, ShellAction),
    /// A window was created: make its native window.
    NewWindow(WindowId),
    /// The user asked to close the window (the close button): ask about unsaved documents,
    /// then close it.
    CloseRequested(WindowId),
    /// Destroy the native window.
    CloseWindow(WindowId),
    /// Leave the message loop.
    Quit,
    /// Show `Progress` on the taskbar button of a window (export and other long work).
    TaskbarProgress(WindowId, ssw_win::taskbar::Progress),
    /// Open the Settings window, or bring the open one to the front.
    OpenSettings,
    /// Something the user did in the Settings window (see [`super::settings_window`]).
    Settings(ssw_ui::shell::settings::SettingsEvent),
    /// Close the Settings window.
    CloseSettings,
    /// A window was activated (the user came back to it): compare its files with the disk
    /// ([`Controller::check_external_changes`]).
    Activated(WindowId),
}

/// The state of one window that does not depend on the operating system.
pub struct WindowCell {
    /// The id workers and requests refer to.
    pub id: WindowId,
    /// The window content.
    pub view: RefCell<ShellView>,
    /// Where the window is (updated by the native window).
    pub placement: Cell<WindowPlacement>,
    /// The paths behind the sidebar's keys, while a workspace is shown.
    pub tree: RefCell<Option<TreePaths>>,
    /// The cursor the pointer position asks for; the loop owner applies it to the window.
    pub cursor: Cell<Cursor>,
    /// The window needs a repaint; the loop owner invalidates it.
    pub dirty: Cell<bool>,
    /// The native window, once it exists: the owner of the print dialog and the share sheet.
    pub hwnd: Cell<Option<windows::Win32::Foundation::HWND>>,
}

impl fmt::Debug for WindowCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WindowCell")
            .field("id", &self.id)
            .field("placement", &self.placement.get())
            .finish_non_exhaustive()
    }
}

/// How a tab close ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// The tab is gone.
    Closed,
    /// The user cancelled the "unsaved changes" question or the save failed.
    Cancelled,
}

/// The windows, tabs and commands of the process (UI thread only).
pub struct Controller {
    /// This controller, for worker callbacks that outlive a borrow (see [`super::import`]).
    me: std::rc::Weak<Controller>,
    core: RefCell<AppCore>,
    pub(super) i18n: Rc<Localizer>,
    docs: Rc<crate::documents::Documents>,
    pub(super) settings: RefCell<Settings>,
    pub(super) router: RefCell<ShortcutRouter>,
    pub(super) dialogs: RefCell<Box<dyn Dialogs>>,
    pub(super) shell: RefCell<Box<dyn Shell>>,
    pub(super) clipboard: SharedClipboard,
    pub(super) clock: Rc<dyn Clock>,
    windows: RefCell<Vec<Rc<WindowCell>>>,
    requests: RefCell<VecDeque<Request>>,
    active_window: Cell<Option<WindowId>>,
    theme: Cell<Theme>,
    next_window: Cell<u64>,
    /// What each window's inspector shows (see [`super::inspector`]).
    pub(super) inspector_feeds: RefCell<super::inspector::InspectorFeeds>,
    /// Counts finished workspace index runs; the inspector's backlinks follow it.
    pub(super) index_epoch: Rc<Cell<u64>>,
    /// What each open document may load from the network (see [`super::remote_images`]).
    pub(super) remote_images: Rc<super::remote_images::RemoteImagePolicy>,
    /// Workspace watchers, index upkeep, `[[` completion and background opens (see
    /// [`super::files`]).
    pub(super) files: super::files::FilesState,
    /// The update check and the help documents (see [`super::help`]).
    pub(super) help: super::help::HelpState,
    /// The print and share seams and the exports in flight (see [`super::export`]).
    pub(super) export: super::export::ExportState,
    /// The Settings window and the persistence of the settings (see
    /// [`super::settings_window`]).
    pub(super) settings_hub: super::settings_window::SettingsHub,
    /// Which banners the user closed, the recovery offer (see [`super::banner_flows`]).
    pub(super) flows: super::banner_flows::FlowState,
}

impl fmt::Debug for Controller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Controller")
            .field("windows", &self.windows.borrow().len())
            .field("active_window", &self.active_window.get())
            .finish_non_exhaustive()
    }
}

impl Controller {
    /// A controller over the application core.
    #[must_use]
    pub fn new(
        mut core: AppCore,
        dialogs: Box<dyn Dialogs>,
        shell: Box<dyn Shell>,
        clipboard: SharedClipboard,
        clock: Rc<dyn Clock>,
    ) -> Rc<Self> {
        let settings = std::mem::take(&mut core.settings);
        let keymap = Keymap::with_overrides(&settings.shortcuts);
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            i18n: Rc::clone(&core.i18n),
            docs: Rc::clone(&core.docs),
            router: RefCell::new(ShortcutRouter::new(keymap)),
            settings: RefCell::new(settings),
            core: RefCell::new(core),
            dialogs: RefCell::new(dialogs),
            shell: RefCell::new(shell),
            clipboard,
            clock,
            windows: RefCell::new(Vec::new()),
            requests: RefCell::new(VecDeque::new()),
            active_window: Cell::new(None),
            theme: Cell::new(Theme::light(DEFAULT_ACCENT)),
            next_window: Cell::new(1),
            inspector_feeds: RefCell::default(),
            index_epoch: Rc::new(Cell::new(0)),
            remote_images: Rc::default(),
            files: super::files::FilesState::default(),
            help: super::help::HelpState::default(),
            export: super::export::ExportState::default(),
            settings_hub: super::settings_window::SettingsHub::default(),
            flows: super::banner_flows::FlowState::default(),
        })
    }

    // ----- shared state --------------------------------------------------------------------

    /// The messages of the active locale.
    #[must_use]
    pub fn i18n(&self) -> &Rc<Localizer> {
        &self.i18n
    }

    /// The registry of open documents.
    #[must_use]
    pub fn docs(&self) -> &Rc<crate::documents::Documents> {
        &self.docs
    }

    /// The application core (workers, autosave, session, watchers).
    #[must_use]
    pub fn core(&self) -> &RefCell<AppCore> {
        &self.core
    }

    /// The user's settings.
    pub fn settings(&self) -> Ref<'_, Settings> {
        self.settings.borrow()
    }

    /// The colors new windows start with.
    #[must_use]
    pub fn theme(&self) -> Theme {
        self.theme.get()
    }

    /// Switches every window to `theme` (system theme or accent changed).
    pub fn set_theme(&self, theme: Theme) {
        self.theme.set(theme);
        for window in self.windows.borrow().iter() {
            window.view.borrow_mut().set_theme(theme);
        }
    }

    /// Routes a key event through the keymap (spec §11, AltGr rule).
    pub fn route_key(
        &self,
        event: &ssw_ui::input::KeyEvent,
        probe: &dyn CharProbe,
        caps: bool,
    ) -> ShortcutOutcome {
        self.router.borrow_mut().route(event, probe, caps)
    }

    /// The shortcut table in force (menus show its chords).
    #[must_use]
    pub fn keymap(&self) -> Keymap {
        self.router.borrow().keymap().clone()
    }

    // ----- requests ------------------------------------------------------------------------

    /// Queues work for the loop owner.
    pub fn push(&self, request: Request) {
        self.requests.borrow_mut().push_back(request);
    }

    /// The next queued request.
    pub fn take_request(&self) -> Option<Request> {
        self.requests.borrow_mut().pop_front()
    }

    // ----- windows -------------------------------------------------------------------------

    /// The windows, oldest first.
    #[must_use]
    pub fn windows(&self) -> Vec<Rc<WindowCell>> {
        self.windows.borrow().clone()
    }

    /// The window with `id`.
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<Rc<WindowCell>> {
        self.windows
            .borrow()
            .iter()
            .find(|window| window.id == id)
            .cloned()
    }

    /// The window that was active last.
    #[must_use]
    pub fn active_window(&self) -> Option<Rc<WindowCell>> {
        let id = self.active_window.get()?;
        self.window(id)
            .or_else(|| self.windows.borrow().last().cloned())
    }

    /// Remembers which window the user works in.
    pub fn set_active_window(&self, id: WindowId) {
        self.active_window.set(Some(id));
    }

    /// Creates the state of a window and asks the loop owner for its native window.
    pub fn create_window(
        &self,
        placement: Option<WindowPlacement>,
        size: Size,
        scale: f32,
    ) -> Rc<WindowCell> {
        let id = WindowId(self.next_window.get());
        self.next_window.set(id.0 + 1);
        let mut view = ShellView::new(ViewConfig {
            localizer: Rc::clone(&self.i18n),
            theme: self.theme.get(),
            size,
            scale,
            clock: Rc::clone(&self.clock),
            clipboard: Rc::clone(&self.clipboard),
            settings: self.settings.borrow().clone(),
        });
        view.overlays_mut().set_keymap(self.keymap());
        let cell = Rc::new(WindowCell {
            id,
            view: RefCell::new(view),
            placement: Cell::new(placement.unwrap_or(DEFAULT_PLACEMENT)),
            tree: RefCell::new(None),
            cursor: Cell::new(Cursor::Arrow),
            dirty: Cell::new(true),
            hwnd: Cell::new(None),
        });
        self.windows.borrow_mut().push(Rc::clone(&cell));
        if self.active_window.get().is_none() {
            self.active_window.set(Some(id));
        }
        self.push(Request::NewWindow(id));
        cell
    }

    /// Creates the windows and tabs of the startup plan. Files that cannot be opened are
    /// reported and skipped.
    pub fn open_plan(&self, plan: &StartupPlan) {
        let size = Size::new(1280.0, 800.0);
        let mut created = Vec::new();
        for planned in &plan.windows {
            let cell = self.create_window(planned.placement, size, 1.0);
            let mut active_key = None;
            for (index, tab) in planned.tabs.iter().enumerate() {
                let entry = match &tab.document {
                    PlannedDocument::File(path) => match self.docs.open(path) {
                        Ok(entry) => entry,
                        Err(error) => {
                            self.report(&error);
                            continue;
                        }
                    },
                    PlannedDocument::RestoredUntitled(id) => {
                        let text = self
                            .docs
                            .env()
                            .store
                            .borrow()
                            .untitled_get(id)
                            .ok()
                            .flatten();
                        match text {
                            Some(text) => self.docs.restore_untitled(id, &text),
                            None => continue,
                        }
                    }
                    PlannedDocument::NewUntitled => self.docs.new_untitled(),
                };
                let key = cell.view.borrow_mut().add_tab(entry, tab.workspace.clone());
                if planned.active_tab == Some(index) {
                    active_key = Some(key);
                }
            }
            if let Some(key) = active_key {
                cell.view.borrow_mut().activate_tab(key);
            }
            // The tab's workspace, or (for a single file) none: editor-only.
            let workspace = cell
                .view
                .borrow()
                .active_tab()
                .and_then(|tab| tab.workspace.clone());
            match workspace {
                Some(root) => self.show_workspace(&cell, &root),
                None => {
                    let mut view = cell.view.borrow_mut();
                    if view.tabs().len() == 1 && view.tabs()[0].entry.path().is_some() {
                        view.set_mode(ShellMode::EditorOnly);
                    }
                }
            }
            if cell.view.borrow().tabs().is_empty() {
                let entry = self.docs.new_untitled();
                cell.view.borrow_mut().add_tab(entry, None);
            }
            created.push(cell);
        }
        if let Some(cell) = plan.active_window.and_then(|index| created.get(index)) {
            self.active_window.set(Some(cell.id));
        }
        for cell in &created {
            self.refresh(cell);
        }
        self.offer_recovery();
    }

    /// The windows as the session stores them.
    #[must_use]
    pub fn session(&self) -> Session {
        let windows = self.windows.borrow();
        let states: Vec<WindowState> = windows
            .iter()
            .map(|cell| {
                let view = cell.view.borrow();
                let active_key = view.active_tab().map(|tab| tab.key);
                WindowState {
                    placement: cell.placement.get(),
                    tabs: view
                        .tabs()
                        .iter()
                        .map(|tab| TabState {
                            entry: Rc::clone(&tab.entry),
                            workspace: tab.workspace.clone(),
                        })
                        .collect(),
                    active_tab: active_key
                        .and_then(|key| view.tabs().iter().position(|tab| tab.key == key)),
                }
            })
            .collect();
        let active = self
            .active_window
            .get()
            .and_then(|id| windows.iter().position(|cell| cell.id == id));
        SessionManager::describe(&states, active)
    }

    // ----- housekeeping --------------------------------------------------------------------

    /// Brings the tab strip, banners and status bar of `cell` up to date with its documents.
    pub fn refresh(&self, cell: &WindowCell) {
        let show_dirty = !self.settings.borrow().general.autosave;
        let mut view = cell.view.borrow_mut();
        view.sync_tabs(show_dirty);
        self.sync_banners(cell.id, &mut view);
        self.sync_update_banner(&mut view);
        self.sync_inspector(cell.id, &mut view);
        view.refresh_status();
    }

    /// Refreshes every window.
    pub fn refresh_all(&self) {
        for cell in self.windows() {
            self.refresh(&cell);
        }
    }

    pub(super) fn report(&self, error: &DocError) {
        let message = self.i18n.text(error.message_id());
        let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n.text("window-error-title"),
            message,
            buttons: MessageBoxButtons::Ok,
            icon: Some(MessageBoxIcon::Error),
        });
    }

    // ----- commands ------------------------------------------------------------------------

    /// Runs `command` for window `id`.
    pub fn run_command(&self, id: WindowId, command: CommandId) -> Dispatch {
        let Some(cell) = self.window(id) else {
            return Dispatch::Unhandled;
        };
        let mut actions = WindowActions {
            controller: self,
            cell: Rc::clone(&cell),
        };
        let outcome = self.core.borrow().commands.dispatch(command, &mut actions);
        self.refresh(&cell);
        outcome
    }

    /// Carries out something the user did in the widgets of window `id`.
    pub fn handle_action(&self, id: WindowId, action: ShellAction) {
        let Some(cell) = self.window(id) else {
            return;
        };
        match action {
            ShellAction::Title(event) => self.title_event(&cell, event),
            ShellAction::Sidebar(event) => self.sidebar_event(&cell, event),
            ShellAction::Banner(HostEvent::Action { action, .. }) => {
                self.banner_action(&cell, action)
            }
            ShellAction::Banner(HostEvent::Dismissed(BannerKind::Update)) => self.dismiss_update(),
            ShellAction::Banner(HostEvent::Dismissed(kind)) => self.banner_dismissed(&cell, kind),
            // The status bar's encoding and line-ending clicks are popups the loop owner shows
            // (`popup_for`); a click with no document open asks for nothing.
            ShellAction::Banner(HostEvent::Resized) | ShellAction::Status(_) => {}
            ShellAction::Editor(_, event) => self.editor_event(&cell, event),
            ShellAction::Inspector(event) => self.inspector_event(&cell, event),
            ShellAction::History(event) => self.history_event(&cell, event),
            ShellAction::Overlay(action) => self.overlay_action(&cell, action),
            // The plain editor menu is a popup: the loop owner shows it.
            ShellAction::EditorContext { .. } => {}
            ShellAction::Clipboard(command) => {
                self.run_command(id, command);
            }
        }
        self.refresh(&cell);
    }

    fn title_event(&self, cell: &Rc<WindowCell>, event: TitleEvent) {
        match event {
            TitleEvent::ToggleSidebar => {
                self.run_command(cell.id, CommandId::ViewToggleSidebar);
            }
            TitleEvent::ToggleInspector => {
                self.run_command(cell.id, CommandId::ViewToggleInspector);
            }
            TitleEvent::Format(toggle) => {
                let command = match toggle {
                    FormatToggle::Bold => CommandId::FormatBold,
                    FormatToggle::Italic => CommandId::FormatItalic,
                    FormatToggle::Underline => CommandId::FormatUnderline,
                };
                self.run_command(cell.id, command);
            }
            TitleEvent::Tabs(TabsEvent::Activated(key)) => self.activate_tab(cell, key),
            TitleEvent::Tabs(TabsEvent::CloseRequested(key)) => {
                self.close_tab(cell, key);
            }
            TitleEvent::Tabs(TabsEvent::NewTab) => {
                self.run_command(cell.id, CommandId::TabNew);
            }
            TitleEvent::Tabs(TabsEvent::Reordered { from, to, .. }) => {
                cell.view.borrow_mut().reorder_tab(from, to);
            }
            TitleEvent::Tabs(TabsEvent::TornOut { key, .. }) => self.tear_out(cell, key),
            // Popup menus need the native window: the loop owner shows them.
            TitleEvent::MenuRequested { .. } | TitleEvent::Tabs(TabsEvent::ContextMenu { .. }) => {}
        }
    }

    fn sidebar_event(&self, cell: &Rc<WindowCell>, event: SidebarEvent) {
        // Renames and deletions are file operations, menus are popups the loop owner shows.
        let Some(event) = self.sidebar_file_event(cell, event) else {
            return;
        };
        if let SidebarEvent::Open { key, new_tab } = event
            && let Some(path) = Self::tree_path(cell, key)
        {
            self.open_path(cell, &path, new_tab);
        }
    }

    fn editor_event(&self, cell: &Rc<WindowCell>, event: EditorEvent) {
        match event {
            EditorEvent::ZoomChanged(percent) => self.set_zoom(percent),
            EditorEvent::Media(MediaEvent::LoadImage { dest, .. }) => {
                self.allow_remote_image(cell, &dest);
            }
            EditorEvent::LinkActivated(LinkTarget::Wiki { target, heading }) => {
                self.activate_wikilink(cell, &target, heading.as_deref());
            }
            EditorEvent::LinkActivated(LinkTarget::Url(url)) => {
                let target = Path::new(&url);
                if url.starts_with("http://") || url.starts_with("https://") {
                    let _ = self.shell.borrow_mut().open_url(&url);
                } else {
                    // The folder is read before the file opens: opening borrows the view
                    // mutably, and the guard of an `if let` scrutinee lives through its body.
                    let base = cell
                        .view
                        .borrow()
                        .active_tab()
                        .and_then(|tab| tab.entry.path())
                        .and_then(|path| path.parent().map(Path::to_path_buf));
                    if let Some(base) = base
                        && target.extension().is_some_and(|ext| {
                            ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown")
                        })
                    {
                        self.open_path(cell, &base.join(target), false);
                    }
                }
            }
            _ => {}
        }
    }

    /// Sets the app-wide zoom on every window and stores it in the settings.
    pub fn set_zoom(&self, percent: u32) {
        let percent = percent.clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT);
        self.settings.borrow_mut().appearance.zoom_percent = percent;
        for window in self.windows.borrow().iter() {
            window.view.borrow_mut().set_zoom_percent(percent);
        }
        self.settings_zoom_changed();
    }

    // ----- tabs and documents --------------------------------------------------------------

    pub(super) fn activate_tab(&self, cell: &Rc<WindowCell>, key: u64) {
        let previous = cell
            .view
            .borrow()
            .active_tab()
            .map(|tab| Rc::clone(&tab.entry));
        if let Some(entry) = previous {
            // The tab switch autosave trigger (spec §4.3).
            self.flush(&entry);
        }
        cell.view.borrow_mut().activate_tab(key);
    }

    fn flush(&self, entry: &DocEntry) {
        if let Some(Err(error)) = self.core.borrow().autosave.flush(entry) {
            self.report(&error);
        }
    }

    /// Activates the tab of `cell` that shows `path`; false when no tab of the window does.
    pub(super) fn activate_shown(&self, cell: &Rc<WindowCell>, path: &Path) -> bool {
        let existing = cell
            .view
            .borrow()
            .tabs()
            .iter()
            .find(|tab| tab.entry.path().is_some_and(|open| same_file(&open, path)))
            .map(|tab| tab.key);
        existing.map(|key| self.activate_tab(cell, key)).is_some()
    }

    /// Opens `path` in window `cell`: an already open tab of the window is activated.
    pub fn open_file(&self, cell: &Rc<WindowCell>, path: &Path) -> Result<(), DocError> {
        if self.activate_shown(cell, path) {
            return Ok(());
        }
        // A OneDrive placeholder downloads while it is read: that happens on a worker.
        if self.begin_hydration(cell, path) {
            return Ok(());
        }
        let entry = self.docs.open(path)?;
        let workspace = cell
            .view
            .borrow()
            .active_tab()
            .and_then(|tab| tab.workspace.clone());
        cell.view
            .borrow_mut()
            .open_tab(entry, workspace, &self.docs);
        Ok(())
    }

    /// A weak reference to this controller (for worker callbacks).
    pub(super) fn handle(&self) -> std::rc::Weak<Controller> {
        self.me.clone()
    }

    /// Opens `path` the way the user asked for a document: word-processor files are imported
    /// on a worker (spec §4.1, see [`super::import`]), everything else is read now. Failures
    /// are reported.
    pub(super) fn open_document(&self, cell: &Rc<WindowCell>, path: &Path) {
        if super::import::is_import(path) {
            super::import::open_import(self, cell, path);
        } else if let Err(error) = self.open_file(cell, path) {
            self.report(&error);
        }
    }

    pub(super) fn open_path(&self, cell: &Rc<WindowCell>, path: &Path, _new_tab: bool) {
        use ssw_io::workspace::{FileClass, classify_extension};
        if super::import::is_import(path) {
            self.open_document(cell, path);
            return;
        }
        let class = path.extension().map_or(FileClass::Text, |ext| {
            classify_extension(&ext.to_string_lossy())
        });
        match class {
            FileClass::Markdown | FileClass::Text => {
                if let Err(error) = self.open_file(cell, path) {
                    self.report(&error);
                }
            }
            FileClass::Image | FileClass::Other => {
                let _ = self.shell.borrow_mut().open_path(path);
            }
        }
    }

    /// A new untitled tab in `cell`.
    pub fn new_untitled(&self, cell: &Rc<WindowCell>) {
        let entry = self.docs.new_untitled();
        let workspace = cell
            .view
            .borrow()
            .active_tab()
            .and_then(|tab| tab.workspace.clone());
        cell.view.borrow_mut().add_tab(entry, workspace);
    }

    /// Shows `root` in the sidebar of `cell` and makes it the workspace of the active tab.
    pub fn show_workspace(&self, cell: &Rc<WindowCell>, root: &Path) {
        let fs = Arc::clone(&self.docs.env().fs);
        match WorkspaceTree::open(fs, root) {
            Ok(tree) => {
                *cell.tree.borrow_mut() = Some(tree.paths());
                let mut view = cell.view.borrow_mut();
                view.set_sidebar_options(sidebar_options(&self.settings.borrow()));
                view.set_workspace(Some(Box::new(tree)));
                if let Some(tab) = view.active_tab_mut() {
                    tab.workspace = Some(root.to_path_buf());
                }
                if !view.layout().sidebar_visible() || view.layout().mode() == ShellMode::EditorOnly
                {
                    view.toggle_sidebar();
                }
            }
            Err(_) => {
                let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
                    title: self.i18n.text("window-error-title"),
                    message: self.i18n.format(
                        "window-folder-missing",
                        &[("path", Arg::Str(&root.to_string_lossy()))],
                    ),
                    buttons: MessageBoxButtons::Ok,
                    icon: Some(MessageBoxIcon::Error),
                });
            }
        }
    }

    /// Closes the tab `key` of `cell`: saves (or asks) first, remembers it for "reopen closed
    /// tab", releases the document. The last tab of the last window is replaced by an empty
    /// untitled tab; the last tab of another window closes the window.
    pub fn close_tab(&self, cell: &Rc<WindowCell>, key: u64) -> CloseOutcome {
        let Some(entry) = cell.view.borrow().tab(key).map(|tab| Rc::clone(&tab.entry)) else {
            return CloseOutcome::Closed;
        };
        if !self.confirm_close(cell, key, &entry) {
            return CloseOutcome::Cancelled;
        }
        let Some(tab) = cell.view.borrow_mut().close_tab(key) else {
            return CloseOutcome::Closed;
        };
        let view_state = {
            let document = entry.document().borrow();
            ViewState {
                caret: document.primary_selection().head,
                scroll_anchor: 0,
                scroll_delta: tab.editor.scroll_y(),
                folds: Vec::new(),
            }
        };
        let _ = self
            .core
            .borrow()
            .session
            .tab_closed(&entry, tab.workspace.clone(), &view_state);
        drop(tab);
        self.docs.release(&entry);
        if cell.view.borrow().tabs().is_empty() {
            if self.windows.borrow().len() > 1 {
                self.close_window(cell.id);
            } else {
                self.new_untitled(cell);
            }
        }
        CloseOutcome::Closed
    }

    /// Whether the tab `key` (showing `entry`) may close: with autosave the document is
    /// flushed, otherwise unsaved changes are asked about (save, discard, cancel). Returns
    /// false when the user cancelled or the save failed.
    fn confirm_close(&self, cell: &Rc<WindowCell>, key: u64, entry: &Rc<DocEntry>) -> bool {
        if self.settings.borrow().general.autosave {
            if let Some(Err(error)) = self.core.borrow().autosave.flush(entry) {
                self.report(&error);
                return false;
            }
            return true;
        }
        if !entry.is_dirty() || entry.users() > 1 {
            return true;
        }
        let title = entry.title(&self.i18n);
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n.text("window-unsaved-title"),
            message: self
                .i18n
                .format("window-unsaved-message", &[("name", Arg::Str(&title))]),
            buttons: MessageBoxButtons::SaveDontSaveCancel,
            icon: Some(MessageBoxIcon::Warning),
        });
        match answer {
            Ok(MessageBoxResult::Save) => self.save_for_close(cell, key, entry),
            Ok(MessageBoxResult::DontSave) => true,
            _ => false,
        }
    }

    /// The close button or File → Exit asked to close window `id`: every tab is confirmed as
    /// for a tab close. Returns false when the user cancelled (the window stays open). The
    /// window is not closed here: call [`Controller::close_window`] when this returns true.
    pub fn request_close_window(&self, id: WindowId) -> bool {
        let Some(cell) = self.window(id) else {
            return true;
        };
        let tabs: Vec<(u64, Rc<DocEntry>)> = cell
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| (tab.key, Rc::clone(&tab.entry)))
            .collect();
        tabs.iter()
            .all(|(key, entry)| self.confirm_close(&cell, *key, entry))
    }

    fn save_for_close(&self, cell: &Rc<WindowCell>, key: u64, entry: &Rc<DocEntry>) -> bool {
        cell.view.borrow_mut().activate_tab(key);
        !matches!(
            self.run_command(cell.id, CommandId::FileSave),
            Dispatch::Failed(_) | Dispatch::Unhandled
        ) && !entry.is_dirty()
    }

    /// Removes window `id` (its tabs release their documents) and asks the loop owner to
    /// destroy the native window.
    pub fn close_window(&self, id: WindowId) {
        let Some(cell) = self.window(id) else {
            return;
        };
        self.core.borrow().workers.cancel_window(id);
        while let Some(tab) = close_first_tab(&cell) {
            self.docs.release(&tab.entry);
        }
        self.windows.borrow_mut().retain(|window| window.id != id);
        self.inspector_feeds.borrow_mut().forget(id);
        if self.active_window.get() == Some(id) {
            self.active_window
                .set(self.windows.borrow().last().map(|window| window.id));
        }
        self.push(Request::CloseWindow(id));
    }

    /// A tab dragged out of the strip becomes its own window.
    fn tear_out(&self, cell: &Rc<WindowCell>, key: u64) {
        if cell.view.borrow().tabs().len() < 2 {
            return;
        }
        let Some(tab) = cell.view.borrow_mut().close_tab(key) else {
            return;
        };
        let size = cell.view.borrow().layout().size();
        let scale = cell.view.borrow().layout().scale();
        let fresh = self.create_window(None, size, scale);
        fresh
            .view
            .borrow_mut()
            .add_tab(Rc::clone(&tab.entry), tab.workspace.clone());
        self.active_window.set(Some(fresh.id));
        self.refresh(&fresh);
    }

    /// Moves every tab of the other windows into `cell` (Window → Merge all windows).
    fn merge_all(&self, cell: &Rc<WindowCell>) {
        for other in self.windows() {
            if other.id == cell.id {
                continue;
            }
            while let Some(tab) = close_first_tab(&other) {
                cell.view
                    .borrow_mut()
                    .add_tab(Rc::clone(&tab.entry), tab.workspace.clone());
            }
            self.windows
                .borrow_mut()
                .retain(|window| window.id != other.id);
            self.push(Request::CloseWindow(other.id));
        }
    }

    /// A launch (forwarded by a later process or a Jump List item): files become tabs of the
    /// active window, folders and `new_window` get a window.
    pub fn open_launch(&self, launch: &crate::args::Launch) {
        let size = Size::new(1280.0, 800.0);
        let target = if launch.new_window {
            None
        } else {
            self.active_window()
        };
        let target = match target {
            Some(cell) => cell,
            None if launch.files().next().is_some() || launch.new_window => {
                let cell = self.create_window(None, size, 1.0);
                self.active_window.set(Some(cell.id));
                cell
            }
            None => return,
        };
        for path in launch.files() {
            self.open_document(&target, path);
        }
        for folder in launch.folders() {
            let fresh = self.create_window(None, size, 1.0);
            self.new_untitled(&fresh);
            self.show_workspace(&fresh, folder);
            self.active_window.set(Some(fresh.id));
            self.refresh(&fresh);
        }
        self.refresh(&target);
    }

    fn choose_save(&self, entry: &DocEntry) -> Option<PathBuf> {
        let title = entry.title(&self.i18n);
        let options = SaveFileOptions {
            title: self.i18n.text("window-save-as-title"),
            filters: vec![
                FileFilter {
                    name: self.i18n.text("window-filter-markdown"),
                    extensions: vec!["md".to_owned(), "markdown".to_owned()],
                },
                FileFilter {
                    name: self.i18n.text("window-filter-all"),
                    extensions: vec!["*".to_owned()],
                },
            ],
            suggested_name: Some(format!("{title}.md")),
            default_extension: Some("md".to_owned()),
            initial_dir: entry
                .path()
                .and_then(|path| path.parent().map(Path::to_path_buf)),
        };
        self.dialogs.borrow_mut().save_file(&options).ok().flatten()
    }

    fn open_dialog(&self) -> Vec<PathBuf> {
        let options = OpenFileOptions {
            title: self.i18n.text("window-open-title"),
            filters: vec![FileFilter {
                name: self.i18n.text("window-filter-all"),
                extensions: vec!["*".to_owned()],
            }],
            allow_multiple: true,
            initial_dir: None,
        };
        self.dialogs
            .borrow_mut()
            .open_file(&options)
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    fn folder_dialog(&self) -> Option<PathBuf> {
        let options = PickFolderOptions {
            title: self.i18n.text("window-open-folder-title"),
            initial_dir: None,
        };
        self.dialogs
            .borrow_mut()
            .pick_folder(&options)
            .ok()
            .flatten()
    }
}

/// Generation of the folder searches (a newer search supersedes an older one).
static SEARCH_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The worker key of the folder search of window `id`.
fn search_key(id: WindowId) -> JobKey {
    JobKey(u64::MAX - id.0)
}

/// Removes the leftmost tab of `cell` and returns it; `None` when the window has no tab.
fn close_first_tab(cell: &WindowCell) -> Option<TabView> {
    let key = cell.view.borrow().tabs().first()?.key;
    cell.view.borrow_mut().close_tab(key)
}

/// The work of a folder search (it runs on a worker): the files with matches and the totals,
/// or `None` for a pattern that is empty or not a valid regular expression. `cancelled` is
/// asked at every file with matches; once it says yes the search stops at the next file.
#[must_use]
pub fn search_job(
    root: &Path,
    options: &ssw_io::folder_search::SearchOptions,
    cancelled: &dyn Fn() -> bool,
) -> Option<(
    Vec<ssw_io::folder_search::FileMatches>,
    ssw_io::folder_search::SearchSummary,
)> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut files = Vec::new();
    let summary =
        ssw_io::folder_search::search_folder(root, options, &|_| false, &cancel, |file| {
            if cancelled() {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            files.push(file);
        })
        .ok()?;
    Some((files, summary))
}

impl Controller {
    /// Runs the find, Go to line, Quick Open, palette and folder search commands of `cell`.
    /// Returns false for any other command.
    fn run_overlay_command(&self, cell: &Rc<WindowCell>, command: CommandId) -> bool {
        use ssw_ui::shell::quick_open::QuickOpenMode;
        match command {
            CommandId::SearchFind => cell.view.borrow_mut().overlays_mut().open_find(false),
            CommandId::SearchReplace => cell.view.borrow_mut().overlays_mut().open_find(true),
            CommandId::SearchGoToLine => cell.view.borrow_mut().overlays_mut().open_goto(),
            CommandId::SearchFindNext => cell.view.borrow_mut().overlays_mut().find_next(),
            CommandId::SearchFindPrevious => cell.view.borrow_mut().overlays_mut().find_previous(),
            CommandId::SearchQuickOpen => self.open_quick_open(cell, QuickOpenMode::Files),
            CommandId::SearchCommandPalette => self.open_quick_open(cell, QuickOpenMode::Commands),
            CommandId::SearchInFolder => {
                let seed = cell
                    .view
                    .borrow()
                    .active_tab()
                    .map(|tab| tab.editor.selected_text());
                let mut view = cell.view.borrow_mut();
                if !view.layout().sidebar_visible() {
                    view.toggle_sidebar();
                }
                view.overlays_mut().open_folder_search(seed.as_deref());
            }
            _ => return false,
        }
        cell.view.borrow_mut().requests_mut().invalidate_all();
        cell.dirty.set(true);
        true
    }

    /// Ctrl+E / Ctrl+Shift+P: lists the workspace's files (open documents count as recent)
    /// and the headings of the active document.
    fn open_quick_open(
        &self,
        cell: &Rc<WindowCell>,
        mode: ssw_ui::shell::quick_open::QuickOpenMode,
    ) {
        let (root, toc) = {
            let view = cell.view.borrow();
            let tab = view.active_tab();
            let toc = tab
                .map(|tab| {
                    let doc = tab.entry.document().borrow();
                    ssw_core::toc::Toc::build(doc.ast(), &doc.text())
                })
                .unwrap_or_default();
            (tab.and_then(|tab| tab.workspace.clone()), toc)
        };
        let show_hidden = self.settings.borrow().sidebar.show_hidden_items;
        let files = root
            .as_deref()
            .map(|root| list_workspace_files(root, show_hidden))
            .unwrap_or_default();
        let recent: Vec<usize> = root
            .as_deref()
            .map(|root| {
                self.docs
                    .all()
                    .iter()
                    .filter_map(|entry| entry.path())
                    .filter_map(|path| {
                        let relative = path
                            .strip_prefix(root)
                            .ok()?
                            .to_string_lossy()
                            .replace('\\', "/");
                        files.iter().position(|file| *file == relative)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // The command mode lists every command, those that do not apply drawn disabled: the
        // same enablement as the menus.
        let state = self.menu_state(cell);
        let mut view = cell.view.borrow_mut();
        let overlays = view.overlays_mut();
        overlays
            .quick_open_mut()
            .set_command_availability(&|command| state.enabled(command));
        overlays.open_quick_open(mode, files, recent, &toc);
    }

    /// What the menus and the command palette need to know about window `cell`: the toggles'
    /// check marks and which commands apply.
    #[must_use]
    pub fn menu_state(&self, cell: &WindowCell) -> MenuState {
        let view = cell.view.borrow();
        let layout = view.layout();
        let active = view.active_tab();
        let (typewriter, focus_mode) = active.map_or((false, false), |tab| {
            (tab.editor.typewriter(), tab.editor.focus_mode())
        });
        let word_wrap = active.is_some_and(|tab| {
            let config = tab.editor.config();
            config.document_kind == DocumentKind::PlainText && config.wrap
        });
        MenuState {
            has_document: active.is_some(),
            save_back: active.is_some_and(|tab| tab.entry.can_save_back()),
            plain_file: active.is_some_and(|tab| tab.entry.file_mode() == DocMode::Plain),
            render_markdown: active.is_some_and(|tab| tab.entry.render_as_markdown()),
            word_wrap,
            sidebar: layout.sidebar_visible(),
            inspector: layout.inspector_visible(),
            status_bar: layout.status_bar_visible(),
            format_pill: view.titlebar().pill_visible(),
            typewriter,
            focus_mode,
            full_screen: layout.mode() == ShellMode::FullScreen,
            zen: layout.mode() == ShellMode::Zen,
            other_windows: self.windows.borrow().len() > 1,
        }
    }

    /// Carries out what an overlay asked for.
    fn overlay_action(&self, cell: &Rc<WindowCell>, action: OverlayAction) {
        let root = cell
            .view
            .borrow()
            .active_tab()
            .and_then(|tab| tab.workspace.clone());
        match action {
            OverlayAction::OpenFile(relative) => {
                if let Some(root) = root {
                    self.open_path(cell, &root.join(relative), false);
                }
            }
            OverlayAction::RunCommand(command) => {
                self.run_command(cell.id, command);
            }
            OverlayAction::FolderSearch(request) => {
                if let Some(root) = root {
                    self.start_folder_search(cell, root, request);
                }
            }
            OverlayAction::FolderSearchCleared => {
                let generation =
                    SEARCH_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.core
                    .borrow()
                    .workers
                    .supersede(search_key(cell.id), generation);
            }
            OverlayAction::OpenMatch(hit) => {
                if let Some(root) = root {
                    self.open_match(cell, &root, &hit);
                }
            }
            OverlayAction::Export(request) => {
                self.export_document_with(cell, request.format, request.options);
            }
        }
    }

    /// Runs `ssw_io::folder_search` on a worker; the panel shows the matches when it is done.
    fn start_folder_search(
        &self,
        cell: &Rc<WindowCell>,
        root: PathBuf,
        request: ssw_ui::shell::folder_search::FolderSearchRequest,
    ) {
        use std::sync::atomic::Ordering;
        let generation = SEARCH_GENERATION.fetch_add(1, Ordering::Relaxed);
        let options = ssw_io::folder_search::SearchOptions {
            pattern: request.pattern,
            case_sensitive: request.case_sensitive,
            whole_word: request.whole_word,
            regex: request.regex,
            show_hidden: self.settings.borrow().sidebar.show_hidden_items,
            ..ssw_io::folder_search::SearchOptions::literal("")
        };
        let spec = JobSpec {
            kind: JobKind::Search,
            window: Some(cell.id),
            key: search_key(cell.id),
            generation,
        };
        let job_root = root.clone();
        let target = Rc::clone(cell);
        let _ = self.core.borrow().workers.submit(
            spec,
            move |token| search_job(&job_root, &options, &|| token.is_cancelled()),
            move |outcome| {
                let Ok(Some((files, summary))) = outcome else {
                    return;
                };
                target.view.borrow_mut().overlays_mut().set_folder_results(
                    panel_files(&root, files),
                    ssw_ui::shell::folder_search::FolderSearchSummary {
                        truncated: summary.truncated,
                    },
                );
                target.dirty.set(true);
            },
        );
    }

    /// Opens the file of a folder search match and selects the match.
    fn open_match(
        &self,
        cell: &Rc<WindowCell>,
        root: &Path,
        hit: &ssw_ui::shell::folder_search::FolderSearchHit,
    ) {
        self.open_path(cell, &root.join(&hit.path), false);
        let mut view = cell.view.borrow_mut();
        let Some(tab) = view.active_tab_mut() else {
            return;
        };
        let selection = {
            let doc = tab.entry.document().borrow();
            let text = doc.text();
            let line = doc
                .buffer()
                .line_range(usize::try_from(hit.line.saturating_sub(1)).unwrap_or(0));
            let start = line.start + hit.range.start;
            let end = (line.start + hit.range.end).min(line.end);
            if start <= end && text.is_char_boundary(start) && text.is_char_boundary(end) {
                ssw_core::edit::Selection::new(start, end)
            } else {
                ssw_core::edit::Selection::caret(line.start)
            }
        };
        tab.editor.go_to(selection);
    }
}

/// Whether two paths name the same file (case-insensitive on Windows, canonical when possible).
fn same_file(a: &Path, b: &Path) -> bool {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    canonical(a)
        .to_string_lossy()
        .eq_ignore_ascii_case(&canonical(b).to_string_lossy())
}

/// [`AppActions`] of one window.
struct WindowActions<'a> {
    controller: &'a Controller,
    cell: Rc<WindowCell>,
}

impl AppActions for WindowActions<'_> {
    fn active_document(&self) -> Option<Rc<DocEntry>> {
        self.cell
            .view
            .borrow()
            .active_tab()
            .map(|tab| Rc::clone(&tab.entry))
    }

    fn choose_save_path(&mut self, entry: &DocEntry) -> Option<PathBuf> {
        self.controller.choose_save(entry)
    }

    fn report_error(&mut self, error: &DocError) {
        self.controller.report(error);
    }

    fn run_in_editor(&mut self, command: CommandId) -> bool {
        if let Some(handled) = self.controller.clipboard_command(&self.cell, command) {
            return handled;
        }
        let mut view = self.cell.view.borrow_mut();
        if command == CommandId::FormatLink {
            view.open_link_popup();
            return true;
        }
        view.run_table_command(command)
            || view
                .active_editor_mut()
                .is_some_and(|editor| editor.execute(command))
    }

    fn run_view(&mut self, command: CommandId) -> bool {
        let controller = self.controller;
        match command {
            CommandId::ViewToggleSidebar => {
                // A single file's window shows the file's folder (spec §1).
                let folder = {
                    let view = self.cell.view.borrow();
                    let hidden = !view.layout().sidebar_visible();
                    let no_workspace = self.cell.tree.borrow().is_none();
                    let file = view.active_tab().and_then(|tab| tab.entry.path());
                    (hidden && no_workspace)
                        .then_some(file)
                        .flatten()
                        .and_then(|path| path.parent().map(Path::to_path_buf))
                };
                match folder {
                    Some(folder) => controller.show_workspace(&self.cell, &folder),
                    None => self.cell.view.borrow_mut().toggle_sidebar(),
                }
                true
            }
            CommandId::ViewToggleInspector => {
                self.cell.view.borrow_mut().toggle_inspector();
                true
            }
            CommandId::ViewToggleStatusBar => {
                self.cell.view.borrow_mut().toggle_status_bar();
                true
            }
            CommandId::ViewToggleFormatPill => {
                self.cell.view.borrow_mut().toggle_format_pill();
                true
            }
            CommandId::ViewFullScreen | CommandId::ViewZenMode => {
                let target = if command == CommandId::ViewZenMode {
                    ShellMode::Zen
                } else {
                    ShellMode::FullScreen
                };
                let mut view = self.cell.view.borrow_mut();
                let next = if view.layout().mode() == target {
                    ShellMode::Normal
                } else {
                    target
                };
                view.set_mode(next);
                true
            }
            CommandId::ViewNextPane => {
                self.cell.view.borrow_mut().next_pane(true);
                true
            }
            // Spec §4.1: the plain-text file's own switches; they decline other documents.
            CommandId::ViewWordWrap => self.cell.view.borrow_mut().toggle_word_wrap(),
            CommandId::ViewRenderAsMarkdown => {
                self.cell.view.borrow_mut().toggle_render_as_markdown()
            }
            CommandId::ViewZoomIn | CommandId::ViewZoomOut | CommandId::ViewZoomReset => {
                let current = controller.settings.borrow().appearance.zoom_percent;
                let next = match command {
                    CommandId::ViewZoomIn => current + ZOOM_STEP_PERCENT,
                    CommandId::ViewZoomOut => current.saturating_sub(ZOOM_STEP_PERCENT),
                    _ => 100,
                };
                controller.set_zoom(next);
                true
            }
            _ => controller.run_overlay_command(&self.cell, command) || self.run_in_editor(command),
        }
    }

    fn run_app(&mut self, command: CommandId) -> bool {
        let controller = self.controller;
        let cell = Rc::clone(&self.cell);
        let active_key = cell.view.borrow().active_tab().map(|tab| tab.key);
        match command {
            CommandId::FileNew | CommandId::TabNew => {
                controller.new_untitled(&cell);
                true
            }
            CommandId::FileNewWindow => {
                let (size, scale) = {
                    let view = cell.view.borrow();
                    (view.layout().size(), view.layout().scale())
                };
                let fresh = controller.create_window(None, size, scale);
                controller.new_untitled(&fresh);
                controller.set_active_window(fresh.id);
                true
            }
            CommandId::FileOpen => {
                for path in controller.open_dialog() {
                    controller.open_path(&cell, &path, true);
                }
                true
            }
            CommandId::FileOpenFolder => {
                if let Some(folder) = controller.folder_dialog() {
                    controller.show_workspace(&cell, &folder);
                }
                true
            }
            CommandId::TabClose => {
                if let Some(key) = active_key {
                    controller.close_tab(&cell, key);
                }
                true
            }
            CommandId::TabCloseOthers | CommandId::TabCloseToRight => {
                let Some(key) = active_key else {
                    return true;
                };
                let action = if command == CommandId::TabCloseOthers {
                    ssw_ui::shell::tabs::TabAction::CloseOthers
                } else {
                    ssw_ui::shell::tabs::TabAction::CloseToRight
                };
                let targets = cell
                    .view
                    .borrow()
                    .titlebar()
                    .tabs()
                    .close_targets(action, key);
                for target in targets {
                    controller.close_tab(&cell, target);
                }
                true
            }
            CommandId::TabReopenClosed => {
                let reopened = controller.core.borrow().session.reopen_closed();
                if let Some((entry, workspace)) = reopened {
                    cell.view.borrow_mut().add_tab(entry, workspace);
                }
                true
            }
            CommandId::TabNext | CommandId::TabPrevious => {
                let keys = cell.view.borrow().titlebar().tabs().keys();
                if let (Some(active), true) = (active_key, keys.len() > 1)
                    && let Some(index) = keys.iter().position(|&key| key == active)
                {
                    let count = keys.len();
                    let next = if command == CommandId::TabNext {
                        (index + 1) % count
                    } else {
                        (index + count - 1) % count
                    };
                    controller.activate_tab(&cell, keys[next]);
                }
                true
            }
            CommandId::TabCopyPath => {
                if let Some(path) = self.active_document().and_then(|entry| entry.path()) {
                    let content = ssw_ui::services::ClipboardContent::from_text(
                        path.to_string_lossy().into_owned(),
                    );
                    let _ = controller.clipboard.borrow_mut().set(content);
                }
                true
            }
            CommandId::TabRevealInExplorer => {
                if let Some(path) = self.active_document().and_then(|entry| entry.path()) {
                    let _ = controller.shell.borrow_mut().reveal_in_explorer(&path);
                }
                true
            }
            CommandId::WindowMergeAll => {
                controller.merge_all(&cell);
                true
            }
            CommandId::FileExit => {
                controller.push(Request::Quit);
                true
            }
            CommandId::HelpWelcome
            | CommandId::HelpKeyboardShortcuts
            | CommandId::HelpReportIssue
            | CommandId::HelpCheckForUpdates
            | CommandId::HelpAbout => controller.run_help_command(&cell, command),
            CommandId::SettingsOpen => {
                controller.push(Request::OpenSettings);
                true
            }
            CommandId::SettingsOpenLogsFolder => {
                controller.open_logs_folder();
                true
            }
            CommandId::SettingsReset => {
                controller.reset_settings();
                true
            }
            CommandId::FileVersionHistory => {
                controller.open_history(cell.id, None);
                true
            }
            CommandId::FileExport => {
                controller.open_export_dialog(&cell);
                true
            }
            CommandId::FilePrint => {
                controller.print_document(&cell);
                true
            }
            CommandId::FileShare => {
                controller.share_document(&cell);
                true
            }
            other => match super::export::format_of(other) {
                Some(format) => {
                    controller.export_document(&cell, format);
                    true
                }
                // Settings, help and the sidebar context commands are handled elsewhere.
                None => false,
            },
        }
    }
}
