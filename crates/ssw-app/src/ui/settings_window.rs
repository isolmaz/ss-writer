//! The Settings window (spec §10) and the life of the settings behind it.
//!
//! - [`SettingsView`] / [`SettingsCell`]: the platform-neutral content of the second top-level
//!   window. It hosts [`ssw_ui::shell::settings::SettingsWindow`] below the caption strip and
//!   drives it with [`ssw_ui::input::Event`]s, like [`super::shell_view::ShellView`] does for the
//!   main windows.
//! - [`SettingsHub`] and the `impl Controller` below: every change the window reports is merged
//!   into the controller's settings, applied live to every open window (theme, fonts, zoom,
//!   editor options, spelling, shortcuts, sidebar, autosave) and written to `settings.json`
//!   (atomic, debounced). Reset, "open settings.json" and the logs folder are answered here.
//!   `settings.json` edited by hand and saved is read back and applied too.
//! - [`SettingsNative`]: the native window ([`ssw_win::window::WindowHost`]) with its
//!   [`WindowHandler`] and UI Automation bridge. The loop owner ([`super::host::UiApp`]) makes
//!   and destroys it from [`Request::OpenSettings`] / [`Request::CloseSettings`].
//!
//! Window callbacks only push [`Request`]s; the work (dialogs, files, applying) runs in the
//! loop owner, outside of any window procedure.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use serde_json::{Map, Value};
use ssw_core::command::CommandId;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::settings::{AccentMode, Appearance, Settings, Theme as ThemeSetting};
use ssw_io::logging::{Level, Logger};
use ssw_io::settings_file::{SettingsFileError, save_settings};
use ssw_ui::access::{AccessAction, AccessTree};
use ssw_ui::canvas::Canvas;
use ssw_ui::editor::spell::SpellCheck;
use ssw_ui::editor::{EditorConfig, MAX_ZOOM_PERCENT, MIN_ZOOM_PERCENT};
use ssw_ui::geometry::{Color, Point, Rect, Size};
use ssw_ui::input::{Cursor, Event, PointerEventKind, vk};
use ssw_ui::services::{MessageBoxButtons, MessageBoxOptions, MessageBoxResult, PickFolderOptions};
use ssw_ui::shell::settings::{
    FieldId, KeyNames, LanguageChoice, SettingsEnv, SettingsEvent, SettingsWindow,
};
use ssw_ui::shortcuts::CharProbe;
use ssw_ui::text::{FontWeight, TextLayoutEngine, TextStyle};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme, ThemeMode};
use ssw_ui::widget::{
    AccessCx, EventCx, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests, Widget, WidgetId,
};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;
use ssw_win::canvas::{CanvasAssets, ImageStore};
use ssw_win::chrome::CAPTION_HEIGHT;
use ssw_win::com::WinError;
use ssw_win::gfx::GfxDevices;
use ssw_win::keyboard::key_name;
use ssw_win::theme::SystemTheme;
use ssw_win::uia::{ActionSink, UiaBridge};
use ssw_win::wic::Wic;
use ssw_win::window::{
    HitRegion, WakeHandle, WindowHandler, WindowHost, WindowInfo, WindowOptions, default_hit_region,
};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::WM_GETOBJECT;

use super::controller::{Controller, Request};
use super::shell_view::sidebar_options;
use super::window::WindowEnv;
use crate::autosave::AutosavePolicy;
use crate::i18n::{embedded, localizer_for, resolve_locale};

/// How long after the last change the settings are written.
const DEBOUNCE: Duration = Duration::from_millis(400);
/// How long after a failed write the next attempt is made.
const RETRY: Duration = Duration::from_secs(30);
/// Size of the window in DIPs.
pub const WINDOW_SIZE: Size = Size::new(900.0, 640.0);
/// Em size of the window title in the caption strip.
const TITLE_SIZE: f32 = 12.0;
/// Left padding of the window title.
const TITLE_PAD: f32 = 14.0;

// ----- theme -------------------------------------------------------------------------------

/// The theme the settings ask for over what the system provides: light, dark or the system's
/// choice, with the system accent, the built-in blue or the custom color. High contrast always
/// wins.
#[must_use]
pub fn resolve_theme(appearance: &Appearance, system: &SystemTheme) -> Theme {
    if system.high_contrast {
        return system.theme;
    }
    let dark = match appearance.theme {
        ThemeSetting::System => system.dark,
        ThemeSetting::Light => false,
        ThemeSetting::Dark => true,
    };
    let accent = match appearance.accent {
        AccentMode::System => system.accent,
        AccentMode::Blue => DEFAULT_ACCENT,
        AccentMode::Custom => appearance
            .accent_custom
            .as_deref()
            .and_then(|hex| Color::from_hex(hex).ok())
            .unwrap_or(system.accent),
    };
    if dark {
        Theme::dark(accent)
    } else {
        Theme::light(accent)
    }
}

/// What the system provides when nothing was read from the OS (headless runs).
fn fallback_system() -> SystemTheme {
    SystemTheme {
        theme: Theme::light(DEFAULT_ACCENT),
        dark: false,
        high_contrast: false,
        transparency_effects: true,
        client_animations: true,
        accent: DEFAULT_ACCENT,
    }
}

// ----- merging and resetting ---------------------------------------------------------------

/// `current` with the changes `edited` made against `base` (what the Settings window showed
/// before the edit). Settings the app changed meanwhile (the zoom) are not undone by a window
/// that did not touch them.
#[must_use]
pub fn merge_changes(current: &Settings, base: &Settings, edited: &Settings) -> Settings {
    let mut merged = to_object(current);
    merge_object(&mut merged, &to_object(base), &to_object(edited));
    serde_json::from_value(Value::Object(merged)).unwrap_or_else(|_| edited.clone())
}

/// The JSON object of `settings`. Plain data always serializes to an object (a struct with
/// string keys), so the empty object is only what the type asks for, never what is returned.
fn to_object(settings: &Settings) -> Map<String, Value> {
    serde_json::to_value(settings)
        .unwrap_or_default()
        .as_object()
        .cloned()
        .unwrap_or_default()
}

fn merge_object(
    target: &mut Map<String, Value>,
    base: &Map<String, Value>,
    edited: &Map<String, Value>,
) {
    for key in base.keys().filter(|key| !edited.contains_key(*key)) {
        target.remove(key);
    }
    for (key, new) in edited {
        let old = base.get(key);
        if old == Some(new) {
            continue;
        }
        match (new, old, target.get_mut(key)) {
            (Value::Object(new), Some(Value::Object(old)), Some(Value::Object(slot))) => {
                merge_object(slot, old, new);
            }
            _ => {
                target.insert(key.clone(), new.clone());
            }
        }
    }
}

/// The defaults, with the keys this version does not know (they belong to a newer one) kept.
#[must_use]
pub fn defaults_keeping_unknown(current: &Settings) -> Settings {
    let mut fresh = Settings::default();
    fresh.extra.clone_from(&current.extra);
    fresh.general.extra.clone_from(&current.general.extra);
    fresh.appearance.extra.clone_from(&current.appearance.extra);
    fresh.editor.extra.clone_from(&current.editor.extra);
    fresh.sidebar.extra.clone_from(&current.sidebar.extra);
    fresh
        .attachments
        .extra
        .clone_from(&current.attachments.extra);
    fresh.export.extra.clone_from(&current.export.extra);
    fresh.advanced.extra.clone_from(&current.advanced.extra);
    fresh.shortcuts = current
        .shortcuts
        .iter()
        .filter(|(id, _)| id.parse::<CommandId>().is_err())
        .map(|(id, chords)| (id.clone(), chords.clone()))
        .collect();
    fresh
}

/// Where and how big the Settings window opens: centred over the window it was asked from.
#[must_use]
pub fn window_rect(owner: Option<(i32, i32, i32, i32)>, scale: f32) -> (i32, i32, i32, i32) {
    let width = (WINDOW_SIZE.width * scale).round() as i32;
    let height = (WINDOW_SIZE.height * scale).round() as i32;
    match owner {
        Some((x, y, owner_width, owner_height)) => (
            x + (owner_width - width) / 2,
            y + (owner_height - height) / 2,
            width,
            height,
        ),
        None => (100, 100, width, height),
    }
}

/// The languages the app ships: every locale with Fluent files, named in its own language.
#[must_use]
pub fn shipped_languages() -> Vec<LanguageChoice> {
    let mut tags: Vec<&str> = embedded::FTL_FILES
        .iter()
        .map(|(locale, _, _)| *locale)
        .collect();
    tags.sort_unstable();
    tags.dedup();
    tags.into_iter()
        .map(|tag| LanguageChoice {
            tag: tag.to_owned(),
            name: localizer_for(tag).text("settingsapp-language-name"),
        })
        .collect()
}

/// Names chords with the key names of the user's keyboard layout (`GetKeyNameTextW`).
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsKeyNames;

impl KeyNames for WindowsKeyNames {
    fn chord_text(&self, chord: ssw_core::keymap::KeyChord) -> String {
        chord_text_with(chord, |key| key_name(key, 0).ok())
    }
}

/// `chord` named with `name_of`, the name the system has for a virtual key (if any). A modifier
/// without a name is spelled in English; a chord whose key has none is shown as its stored form.
fn chord_text_with(
    chord: ssw_core::keymap::KeyChord,
    name_of: impl Fn(u16) -> Option<String>,
) -> String {
    let mut parts = Vec::new();
    for (held, key) in [
        (chord.ctrl, vk::CONTROL),
        (chord.shift, vk::SHIFT),
        (chord.alt, vk::MENU),
    ] {
        if held {
            parts.push(name_of(key).unwrap_or_else(|| fallback_modifier(key)));
        }
    }
    if chord.win {
        parts.push("Win".to_owned());
    }
    match name_of(chord.vk) {
        Some(name) => parts.push(name),
        None => return chord.to_string(),
    }
    parts.join("+")
}

fn fallback_modifier(key: u16) -> String {
    match key {
        vk::CONTROL => "Ctrl",
        vk::SHIFT => "Shift",
        _ => "Alt",
    }
    .to_owned()
}

// ----- the view ----------------------------------------------------------------------------

/// The content of the Settings window.
pub struct SettingsView {
    window: SettingsWindow,
    theme: Theme,
    size: Size,
    scale: f32,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
    title: String,
    focus_pending: bool,
}

impl fmt::Debug for SettingsView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsView")
            .field("size", &self.size)
            .field("scale", &self.scale)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl SettingsView {
    /// A view of `settings` in a window of `size` DIPs.
    #[must_use]
    pub fn new(
        settings: Settings,
        loc: Rc<Localizer>,
        env: SettingsEnv,
        theme: Theme,
        size: Size,
        scale: f32,
    ) -> Self {
        let title = loc.text("settings-title");
        Self {
            window: SettingsWindow::new(settings, loc, env),
            theme,
            size,
            scale,
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
            title,
            focus_pending: true,
        }
    }

    /// The settings widget.
    #[must_use]
    pub fn window(&self) -> &SettingsWindow {
        &self.window
    }

    /// The settings widget, mutably.
    pub fn window_mut(&mut self) -> &mut SettingsWindow {
        &mut self.window
    }

    /// The id of the root access node.
    #[must_use]
    pub fn root_id(&self) -> WidgetId {
        self.window.id()
    }

    /// The window title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Switches the colors.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.requests.invalidate_all();
    }

    /// Resizes the view (window DIPs) and changes the DPI scale.
    pub fn resize(&mut self, size: Size, scale: f32) {
        self.size = size;
        self.scale = scale;
        self.requests.invalidate_all();
    }

    /// Shows the texts in the language of `loc`.
    pub fn relocalize(&mut self, loc: Rc<Localizer>) {
        self.title = loc.text("settings-title");
        self.window.relocalize(loc);
        self.requests.invalidate_all();
    }

    /// The host requests (repaint, timers, cursor) the widgets made since the last call.
    pub fn requests_mut(&mut self) -> &mut Requests {
        &mut self.requests
    }

    fn content(&self) -> Rect {
        Rect::new(
            0.0,
            CAPTION_HEIGHT,
            self.size.width,
            (self.size.height - CAPTION_HEIGHT).max(0.0),
        )
    }

    fn arrange(&mut self, text: &dyn TextLayoutEngine) {
        let theme = self.theme;
        let id = self.window.id();
        let bounds = self.content();
        self.focus.begin_tab_order();
        {
            let mut cx = LayoutCx::new(
                id,
                &theme,
                self.scale,
                text,
                &mut self.focus,
                &mut self.requests,
            );
            self.window.arrange(&mut cx, bounds);
        }
        self.focus.commit_tab_order();
        if std::mem::take(&mut self.focus_pending) {
            self.focus.focus_next();
        }
        let _ = self.focus.take_changes();
    }

    /// Lays the view out and paints it. `mica` is false when the backdrop is not transparent:
    /// the caption strip then gets its solid theme color.
    pub fn paint(&mut self, canvas: &mut dyn Canvas, text: &dyn TextLayoutEngine, mica: bool) {
        self.arrange(text);
        let theme = self.theme;
        let tokens = theme.tokens;
        let strip = Rect::new(0.0, 0.0, self.size.width, CAPTION_HEIGHT);
        if !mica {
            canvas.fill_rect(strip, tokens.window_bg);
        }
        let title = text.layout(
            &self.title,
            &TextStyle::new(theme.tokens.metrics.ui_font, TITLE_SIZE)
                .with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        );
        canvas.draw_text_layout(
            title.as_ref(),
            Point::new(TITLE_PAD, (CAPTION_HEIGHT - title.size().height) / 2.0),
            tokens.text,
        );
        let cx = PaintCx::new(
            self.window.id(),
            &theme,
            self.scale,
            text,
            &self.focus,
            &self.capture,
            None,
        );
        self.window.paint(&cx, canvas);
    }

    /// Delivers `event` and returns what the user changed. Tab and Shift+Tab move the focus
    /// when no control wants the key (and no shortcut is being captured).
    pub fn event(&mut self, event: &Event, text: &dyn TextLayoutEngine) -> Vec<SettingsEvent> {
        self.arrange(text);
        // The control under the pointer asks for its own shape; without this reset the shape of
        // the control the pointer left would stay.
        if let Event::Pointer(pointer) = event
            && pointer.kind != PointerEventKind::Leave
        {
            self.requests.set_cursor(Cursor::Arrow);
        }
        let theme = self.theme;
        let id = self.window.id();
        let result = {
            let mut cx = EventCx::new(
                id,
                &theme,
                self.scale,
                text,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            self.window.event(&mut cx, event)
        };
        if let Event::Key(key) = event
            && key.is_down
            && key.vk == vk::TAB
            && !result.handled
            && !self.window.is_capturing_shortcut()
        {
            if key.mods.shift() {
                self.focus.focus_previous();
            } else {
                self.focus.focus_next();
            }
            self.requests.invalidate_all();
        }
        self.arrange(text);
        self.window.take_events()
    }

    /// The accessibility tree of the window.
    #[must_use]
    pub fn access_tree(&self, text: &dyn TextLayoutEngine) -> AccessTree {
        let mut tree = AccessTree::new(self.window.id());
        let theme = self.theme;
        let cx = AccessCx::new(self.window.id(), &theme, self.scale, text, &self.focus);
        self.window.access(&cx, &mut tree);
        tree
    }

    /// Carries out an access action that assistive technology requested on node `id`.
    pub fn perform_access(
        &mut self,
        id: WidgetId,
        action: &AccessAction,
        text: &dyn TextLayoutEngine,
    ) -> Vec<SettingsEvent> {
        self.arrange(text);
        let theme = self.theme;
        {
            let mut cx = EventCx::new(
                id,
                &theme,
                self.scale,
                text,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            self.window.perform(&mut cx, action);
        }
        self.arrange(text);
        self.window.take_events()
    }
}

/// The state of the Settings window that does not depend on the operating system.
pub struct SettingsCell {
    /// The window content.
    pub view: RefCell<SettingsView>,
    /// The settings the window last showed: edits are merged relative to them.
    base: RefCell<Settings>,
    /// The cursor the pointer position asks for; the loop owner applies it to the window.
    pub cursor: Cell<Cursor>,
    /// The window needs a repaint; the loop owner invalidates it.
    pub dirty: Cell<bool>,
}

impl fmt::Debug for SettingsCell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsCell").finish_non_exhaustive()
    }
}

/// What opening the Settings window needs from the platform.
pub struct SettingsSetup {
    /// Asks the keyboard layout whether Ctrl+Alt+key types a character.
    pub probe: Box<dyn CharProbe>,
    /// Names chords for display.
    pub key_names: Box<dyn KeyNames>,
    /// The user's motion preference.
    pub motion: Motion,
    /// Window size in DIPs.
    pub size: Size,
    /// `dpi / 96`.
    pub scale: f32,
}

impl fmt::Debug for SettingsSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsSetup")
            .field("size", &self.size)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

// ----- the hub -----------------------------------------------------------------------------

/// What `settings.json` looked like when last seen: modification time and length.
type Stamp = Option<(SystemTime, u64)>;

fn stamp_of(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    meta.modified().ok().map(|time| (time, meta.len()))
}

/// The Settings window and the persistence state of the settings (a field of [`Controller`]).
#[derive(Default)]
pub struct SettingsHub {
    cell: RefCell<Option<Rc<SettingsCell>>>,
    /// When the pending change is to be written.
    due: Cell<Option<Duration>>,
    /// The last write failed and the user was told.
    failed: Cell<bool>,
    spells: RefCell<Vec<Weak<SpellCheck>>>,
    system: Cell<Option<SystemTheme>>,
    theme_dirty: Cell<bool>,
    /// `settings.json` was opened for editing: edits saved by hand are read back.
    watching: Cell<bool>,
    stamp: Cell<Stamp>,
}

impl fmt::Debug for SettingsHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsHub")
            .field("open", &self.cell.borrow().is_some())
            .field("due", &self.due.get())
            .finish_non_exhaustive()
    }
}

impl Controller {
    // ----- the window ----------------------------------------------------------------------

    /// The Settings window state, while it is open.
    #[must_use]
    pub fn settings_cell(&self) -> Option<Rc<SettingsCell>> {
        self.settings_hub.cell.borrow().clone()
    }

    /// The state of the Settings window: the open one, or a new one showing the current
    /// settings.
    pub fn create_settings(&self, setup: SettingsSetup) -> Rc<SettingsCell> {
        if let Some(cell) = self.settings_cell() {
            return cell;
        }
        let settings = self.settings.borrow().clone();
        let env = SettingsEnv {
            clipboard: Rc::clone(&self.clipboard),
            char_probe: setup.probe,
            key_names: setup.key_names,
            languages: shipped_languages(),
            motion: setup.motion,
        };
        let view = SettingsView::new(
            settings.clone(),
            Rc::clone(self.i18n()),
            env,
            self.theme(),
            setup.size,
            setup.scale,
        );
        let cell = Rc::new(SettingsCell {
            view: RefCell::new(view),
            base: RefCell::new(settings),
            cursor: Cell::new(Cursor::Arrow),
            dirty: Cell::new(true),
        });
        *self.settings_hub.cell.borrow_mut() = Some(Rc::clone(&cell));
        cell
    }

    /// The Settings window was closed: pending changes are written now.
    pub fn close_settings(&self) {
        self.flush_settings();
        self.settings_hub.cell.take();
    }

    /// Queues what the user did in the Settings window, in order, for the loop owner (both the
    /// window procedure and the housekeeping of assistive actions hand their events over here).
    pub fn push_settings(&self, events: impl IntoIterator<Item = SettingsEvent>) {
        for event in events {
            self.push(Request::Settings(event));
        }
    }

    /// Carries out something the user did in the Settings window.
    pub fn settings_event(&self, event: SettingsEvent) {
        match event {
            SettingsEvent::Changed(_) => self.settings_changed(),
            SettingsEvent::BrowseFolder(field) => self.browse_folder(field),
            SettingsEvent::OpenDefaultApps => {
                // The page opening is the whole effect; a refusal has nobody to tell.
                let _ = self.shell.borrow_mut().open_default_apps_settings();
            }
            SettingsEvent::OpenLogsFolder => self.open_logs_folder(),
            SettingsEvent::OpenSettingsFile => self.open_settings_file(),
            SettingsEvent::ResetSettings => self.reset_settings(),
        }
    }

    fn browse_folder(&self, field: FieldId) {
        let options = PickFolderOptions {
            title: self.i18n().text("settingsapp-browse-title"),
            initial_dir: None,
        };
        let picked = self.dialogs.borrow_mut().pick_folder(&options);
        let (Ok(Some(path)), Some(cell)) = (picked, self.settings_cell()) else {
            return;
        };
        let events = {
            let mut view = cell.view.borrow_mut();
            view.window_mut().set_folder(field, &path.to_string_lossy());
            view.requests_mut().invalidate_all();
            view.window_mut().take_events()
        };
        for event in events {
            self.settings_event(event);
        }
    }

    /// Shows `settings` in the Settings window (when open) and remembers them as its base.
    fn show_in_window(&self, settings: &Settings) {
        let Some(cell) = self.settings_cell() else {
            return;
        };
        let mut view = cell.view.borrow_mut();
        view.window_mut().set_settings(settings.clone());
        view.requests_mut().invalidate_all();
        *cell.base.borrow_mut() = settings.clone();
    }

    /// The window reported a change: merge it into the settings, apply and persist it.
    fn settings_changed(&self) {
        let Some(cell) = self.settings_cell() else {
            return;
        };
        let edited = cell.view.borrow().window().settings().clone();
        let current = self.settings.borrow().clone();
        let merged = merge_changes(&current, &cell.base.borrow(), &edited);
        let language_changed = merged.general.language != current.general.language;
        *cell.base.borrow_mut() = edited.clone();
        let installed = self.install_settings(merged, true);
        if installed != edited {
            self.show_in_window(&installed);
        }
        if language_changed {
            let locale = resolve_locale(
                installed.general.language.as_deref(),
                &ssw_win::locale::ui_language(),
            );
            let mut view = cell.view.borrow_mut();
            view.relocalize(Rc::new(localizer_for(&locale)));
        }
    }

    // ----- applying ------------------------------------------------------------------------

    /// Makes `new` the settings in force and applies what differs from the old ones to every
    /// window. `persist` schedules the write to `settings.json`. Returns the settings as stored
    /// (the zoom is kept within what the editor supports).
    pub fn install_settings(&self, mut new: Settings, persist: bool) -> Settings {
        new.appearance.zoom_percent = new
            .appearance
            .zoom_percent
            .clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT);
        let old = self.settings.replace(new.clone());
        if old == new {
            return new;
        }
        self.apply_difference(&old, &new);
        if persist {
            self.schedule_settings_save();
        }
        new
    }

    fn apply_difference(&self, old: &Settings, new: &Settings) {
        if old.shortcuts != new.shortcuts {
            let keymap = new.keymap();
            self.router.borrow_mut().set_keymap(keymap.clone());
            for cell in self.windows() {
                cell.view
                    .borrow_mut()
                    .overlays_mut()
                    .set_keymap(keymap.clone());
            }
        }
        if old.general.autosave != new.general.autosave
            || old.general.autosave_delay_ms != new.general.autosave_delay_ms
            || old.advanced.history_retention_days != new.advanced.history_retention_days
        {
            self.core()
                .borrow()
                .autosave
                .set_policy(AutosavePolicy::from_settings(new));
        }
        if (
            old.appearance.theme,
            old.appearance.accent,
            &old.appearance.accent_custom,
        ) != (
            new.appearance.theme,
            new.appearance.accent,
            &new.appearance.accent_custom,
        ) {
            self.apply_theme(self.resolved_theme(new));
        }
        if old.editor.spell_check != new.editor.spell_check {
            self.settings_hub
                .spells
                .borrow_mut()
                .retain(|weak| weak.strong_count() > 0);
            for check in self
                .settings_hub
                .spells
                .borrow()
                .iter()
                .filter_map(Weak::upgrade)
            {
                check.set_enabled(new.editor.spell_check);
            }
        }
        let config = EditorConfig::from_settings(new);
        let sidebar = sidebar_options(new);
        for cell in self.windows() {
            let mut view = cell.view.borrow_mut();
            view.set_editor_config(config.clone());
            view.set_sidebar_options(sidebar);
            if old.appearance.show_status_bar != new.appearance.show_status_bar {
                view.layout_mut()
                    .set_status_bar_visible(new.appearance.show_status_bar);
            }
            if old.appearance.show_format_pill != new.appearance.show_format_pill
                && view.titlebar().pill_visible() != new.appearance.show_format_pill
            {
                view.toggle_format_pill();
            }
            if old.editor.spell_check != new.editor.spell_check {
                for tab in view.tabs_mut() {
                    tab.editor.invalidate_objects();
                }
            }
            view.requests_mut().invalidate_all();
        }
        // Autosave off shows the dirty dots on the tabs.
        self.refresh_all();
    }

    fn resolved_theme(&self, settings: &Settings) -> Theme {
        let system = self
            .settings_hub
            .system
            .get()
            .unwrap_or_else(fallback_system);
        resolve_theme(&settings.appearance, &system)
    }

    fn apply_theme(&self, theme: Theme) {
        if self.theme() == theme {
            return;
        }
        self.set_theme(theme);
        if let Some(cell) = self.settings_cell() {
            cell.view.borrow_mut().set_theme(theme);
        }
        self.settings_hub.theme_dirty.set(true);
    }

    /// The system theme (or accent) is `system`: the windows show it as the settings allow.
    pub fn set_system_theme(&self, system: SystemTheme) {
        self.settings_hub.system.set(Some(system));
        let theme = self.resolved_theme(&self.settings.borrow());
        self.apply_theme(theme);
    }

    /// The new theme when it changed since the last call (the native windows follow it).
    pub fn take_theme_change(&self) -> Option<Theme> {
        self.settings_hub
            .theme_dirty
            .replace(false)
            .then(|| self.theme())
    }

    /// Whether the Mica backdrop is wanted (`appearance.mica`).
    #[must_use]
    pub fn mica(&self) -> bool {
        self.settings
            .try_borrow()
            .map_or(true, |settings| settings.appearance.mica)
    }

    /// Puts a window's spell checker under the control of `editor.spell_check`.
    pub fn register_spell(&self, check: &Rc<SpellCheck>) {
        check.set_enabled(self.settings.borrow().editor.spell_check);
        self.settings_hub
            .spells
            .borrow_mut()
            .push(Rc::downgrade(check));
    }

    /// Whether the painted blocks of some window wait for a spelling check.
    #[must_use]
    pub fn spell_pending(&self) -> bool {
        self.settings_hub
            .spells
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .any(|check| check.pending())
    }

    /// Runs the waiting spelling checks for about `budget` (each window's service gets the
    /// budget in turn). Returns whether any answered: the editors then repaint to show the
    /// squiggles.
    pub fn pump_spell(&self, budget: Duration) -> bool {
        let checks: Vec<Rc<SpellCheck>> = self
            .settings_hub
            .spells
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        let mut answered = 0;
        for check in checks {
            answered += check.pump(budget);
        }
        answered > 0
    }

    /// The app-wide zoom changed: it is persisted and shown in the Settings window.
    pub(super) fn settings_zoom_changed(&self) {
        self.schedule_settings_save();
        let current = self.settings.borrow().clone();
        let stale = self
            .settings_cell()
            .is_some_and(|cell| cell.view.borrow().window().settings() != &current);
        if stale {
            self.show_in_window(&current);
        }
    }

    // ----- actions -------------------------------------------------------------------------

    /// Settings → Open logs folder.
    pub fn open_logs_folder(&self) {
        let dir = self.core().borrow().paths.logs_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = self.shell.borrow_mut().open_path(&dir);
    }

    /// Settings → Open settings.json: the file is written first (it may not exist yet) and
    /// opened in a tab of the active window; what the user saves there is applied.
    pub fn open_settings_file(&self) {
        let path = self.core().borrow().paths.settings_file();
        self.flush_settings();
        if !path.exists() {
            self.write_settings();
        }
        self.settings_hub.watching.set(true);
        self.settings_hub.stamp.set(stamp_of(&path));
        if let Some(cell) = self.active_window() {
            self.open_path(&cell, &path, true);
        }
    }

    /// Settings → Reset settings: after the user confirms, every setting returns to its
    /// default and the file is written at once.
    pub fn reset_settings(&self) {
        let options = MessageBoxOptions {
            title: self.i18n().text("settingsapp-reset-title"),
            message: self.i18n().text("settingsapp-reset-message"),
            buttons: MessageBoxButtons::YesNo,
            icon: None,
        };
        let answer = self.dialogs.borrow_mut().message_box(&options);
        if !matches!(answer, Ok(MessageBoxResult::Yes)) {
            return;
        }
        let fresh = defaults_keeping_unknown(&self.settings.borrow());
        let installed = self.install_settings(fresh, true);
        self.show_in_window(&installed);
        self.flush_settings();
    }

    // ----- persistence ---------------------------------------------------------------------

    fn schedule_settings_save(&self) {
        self.settings_hub.due.set(Some(self.clock.now() + DEBOUNCE));
    }

    /// Writes a due change and reads back `settings.json` when it was edited by hand. Called by
    /// the loop at every turn.
    pub fn pump_settings(&self) {
        if let Some(due) = self.settings_hub.due.get()
            && self.clock.now() >= due
        {
            self.write_settings();
        }
        self.reload_edited_file();
    }

    /// Writes a pending change now (the window closed, the application exits).
    pub fn flush_settings(&self) {
        if self.settings_hub.due.get().is_some() {
            self.write_settings();
        }
    }

    /// [`Self::flush_settings`] for the end of the session, when no message box can be
    /// answered or opened: a failure is logged and not shown.
    pub(super) fn flush_settings_quietly(&self) {
        if self.settings_hub.due.get().is_some()
            && let Err(error) = self.save_settings_now()
        {
            self.log_settings_failure(&error);
        }
    }

    fn write_settings(&self) {
        if let Err(error) = self.save_settings_now() {
            self.settings_write_failed(&error);
        }
    }

    fn save_settings_now(&self) -> Result<(), SettingsFileError> {
        self.settings_hub.due.set(None);
        let settings = self.settings.borrow().clone();
        let path = self.core().borrow().paths.settings_file();
        save_settings(&path, &settings)?;
        self.settings_hub.failed.set(false);
        if self.settings_hub.watching.get() {
            self.settings_hub.stamp.set(stamp_of(&path));
        }
        Ok(())
    }

    /// Logs the failure and schedules the next attempt.
    fn log_settings_failure(&self, error: &SettingsFileError) {
        self.core().borrow().logger.log(
            Level::Error,
            "settings",
            &format!("settings.json was not written: {error}"),
        );
        self.settings_hub.due.set(Some(self.clock.now() + RETRY));
    }

    fn settings_write_failed(&self, error: &SettingsFileError) {
        self.log_settings_failure(error);
        if self.settings_hub.failed.replace(true) {
            return;
        }
        let reason = error.to_string();
        let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n().text("settingsapp-save-failed-title"),
            message: self.i18n().format(
                "settingsapp-save-failed-message",
                &[("reason", Arg::Str(&reason))],
            ),
            buttons: MessageBoxButtons::Ok,
            icon: Some(ssw_ui::services::MessageBoxIcon::Error),
        });
    }

    /// `settings.json` was saved by hand: its settings are applied (never written back). A
    /// file that does not parse yet (the user is mid-edit) is ignored until it does.
    fn reload_edited_file(&self) {
        if !self.settings_hub.watching.get() || self.settings_hub.due.get().is_some() {
            return;
        }
        let path = self.core().borrow().paths.settings_file();
        let stamp = stamp_of(&path);
        if stamp == self.settings_hub.stamp.get() {
            return;
        }
        self.settings_hub.stamp.set(stamp);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let Ok(parsed) = Settings::from_json_str(text.trim_start_matches('\u{feff}')) else {
            return;
        };
        let installed = self.install_settings(parsed, false);
        self.show_in_window(&installed);
    }
}

// ----- the native window -------------------------------------------------------------------

/// Actions assistive technology asked for, handed from UIA threads to the UI thread.
struct AccessQueue {
    items: Mutex<Vec<(WidgetId, AccessAction)>>,
    wake: Mutex<Option<WakeHandle>>,
}

impl ActionSink for AccessQueue {
    fn perform(&self, id: WidgetId, action: AccessAction) -> bool {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((id, action));
        if let Some(wake) = *self.wake.lock().unwrap_or_else(PoisonError::into_inner) {
            wake.wake();
        }
        true
    }
}

/// Facts the window procedure side leaves for the loop owner.
#[derive(Debug, Default)]
pub struct SettingsLink {
    /// The native window was destroyed.
    pub destroyed: Cell<bool>,
}

/// The UI Automation side of the window, shared by the handler and the loop owner.
struct SettingsAccess {
    queue: Arc<AccessQueue>,
    bridge: RefCell<Option<UiaBridge>>,
    active: Cell<bool>,
}

impl SettingsAccess {
    fn publish(&self, cell: &SettingsCell, text: &dyn TextLayoutEngine) {
        let Ok(view) = cell.view.try_borrow() else {
            return;
        };
        let tree = view.access_tree(text);
        drop(view);
        if let Some(bridge) = self.bridge.borrow().as_ref() {
            let _ = bridge.update(tree);
        }
    }
}

/// The [`WindowHandler`] of the Settings window.
struct SettingsGlue {
    env: Rc<WindowEnv>,
    cell: Rc<SettingsCell>,
    link: Rc<SettingsLink>,
    access: Rc<SettingsAccess>,
}

impl WindowHandler for SettingsGlue {
    fn hit_test(&self, point: Point, info: &WindowInfo) -> HitRegion {
        default_hit_region(point, info.size_dip().width)
    }

    fn on_event(&mut self, _info: &WindowInfo, event: Event) {
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        let events = view.event(&event, &self.env.text);
        drop(view);
        self.env.controller.push_settings(events);
    }

    fn on_paint(&mut self, _info: &WindowInfo, canvas: &mut dyn Canvas) {
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        let mica = self.env.mica.get() && self.env.controller.mica();
        view.paint(canvas, &self.env.text, mica);
        view.requests_mut().take_invalidation();
    }

    fn on_resize(&mut self, info: &WindowInfo) {
        if let Ok(mut view) = self.cell.view.try_borrow_mut() {
            view.resize(info.size_dip(), info.scale());
        }
    }

    fn on_theme_changed(&mut self, _info: &WindowInfo, theme: &SystemTheme) {
        self.env.controller.set_system_theme(*theme);
    }

    fn on_close_requested(&mut self, _info: &WindowInfo) -> bool {
        self.env.controller.push(Request::CloseSettings);
        false
    }

    fn on_destroy(&mut self, _info: &WindowInfo) {
        self.link.destroyed.set(true);
        if let Some(bridge) = self.access.bridge.borrow().as_ref() {
            bridge.disconnect();
        }
    }

    fn on_message(
        &mut self,
        _info: &WindowInfo,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        if msg != WM_GETOBJECT {
            return None;
        }
        if !self.access.active.replace(true) {
            self.access.publish(&self.cell, &self.env.text);
        }
        self.access
            .bridge
            .borrow()
            .as_ref()
            .and_then(|bridge| bridge.handle_get_object(wparam, lparam))
    }
}

/// The native Settings window with everything that belongs to it.
pub struct SettingsNative {
    cell: Rc<SettingsCell>,
    link: Rc<SettingsLink>,
    access: Rc<SettingsAccess>,
    env: Rc<WindowEnv>,
    // Declared last: the host destroys the window (and drops its handler) first.
    host: WindowHost,
}

impl fmt::Debug for SettingsNative {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsNative")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl SettingsNative {
    /// Makes the native window for `cell`. `rect` is the position and size in pixels
    /// ([`window_rect`]); `hidden` windows are never shown or activated (tests).
    ///
    /// # Errors
    /// The window or its graphics cannot be created.
    pub fn create(
        env: &Rc<WindowEnv>,
        devices: &GfxDevices,
        wic: &Wic,
        logger: &Arc<Logger>,
        cell: Rc<SettingsCell>,
        rect: (i32, i32, i32, i32),
        hidden: bool,
    ) -> Result<Self, WinError> {
        let link = Rc::new(SettingsLink::default());
        let access = Rc::new(SettingsAccess {
            queue: Arc::new(AccessQueue {
                items: Mutex::new(Vec::new()),
                wake: Mutex::new(None),
            }),
            bridge: RefCell::new(None),
            active: Cell::new(false),
        });
        let glue = SettingsGlue {
            env: Rc::clone(env),
            cell: Rc::clone(&cell),
            link: Rc::clone(&link),
            access: Rc::clone(&access),
        };
        let theme = env.controller.theme();
        let options = WindowOptions {
            title: cell.view.borrow().title().to_owned(),
            rect: Some(rect),
            dark: theme.mode == ThemeMode::Dark,
            no_activate: hidden,
        };
        let host = WindowHost::create(&options, Box::new(glue), theme)?;
        host.set_assets(CanvasAssets::with_scene_renderer(
            ImageStore::new(wic.clone()),
            env.text.clone(),
            wic.clone(),
        )?);
        host.attach_graphics(devices.clone())?;
        let info = host.info();
        cell.view.borrow_mut().resize(info.size_dip(), info.scale());
        let hwnd = host.hwnd();
        *access
            .queue
            .wake
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(WakeHandle::new(hwnd));
        *access.bridge.borrow_mut() = Some(UiaBridge::new(
            hwnd,
            Arc::clone(&access.queue) as Arc<dyn ActionSink>,
            cell.view.borrow().root_id(),
        ));
        logger.log(Level::Info, "settings", "settings window created");
        if !hidden {
            host.show();
        }
        Ok(Self {
            cell,
            link,
            access,
            env: Rc::clone(env),
            host,
        })
    }

    /// The native handle.
    #[must_use]
    pub fn hwnd(&self) -> windows::Win32::Foundation::HWND {
        self.host.hwnd()
    }

    /// Whether the native window is gone (destroyed by the system).
    #[must_use]
    pub fn destroyed(&self) -> bool {
        self.link.destroyed.get()
    }

    /// Brings the window to the front (the user opened Settings again).
    pub fn bring_to_front(&self) {
        self.host.bring_to_front();
    }

    /// The window follows a theme the controller switched to.
    pub fn apply_theme(&self, theme: Theme) {
        ssw_win::window::apply_dark_mode(self.host.hwnd(), theme.mode == ThemeMode::Dark);
        self.host.set_theme(theme);
        self.cell.dirty.set(true);
    }

    /// Delivers a pending animation frame; returns whether one was delivered.
    pub fn deliver_frame(&self) -> bool {
        self.host.deliver_animation_frame()
    }

    /// Applies what the widgets asked of the native window (repaint, timers, cursor) and
    /// answers assistive technology. Returns the events the assistive actions produced.
    pub fn sync(&self) -> Vec<SettingsEvent> {
        if self.link.destroyed.get() {
            return Vec::new();
        }
        let queued: Vec<(WidgetId, AccessAction)> = std::mem::take(
            &mut *self
                .access
                .queue
                .items
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let mut events = Vec::new();
        let (repaint, cursor) = {
            let Ok(mut view) = self.cell.view.try_borrow_mut() else {
                return events;
            };
            for (id, action) in queued {
                events.extend(view.perform_access(id, &action, &self.env.text));
            }
            let requests = view.requests_mut();
            let repaint = !requests.take_invalidation().is_clean();
            let cursor = requests.take_cursor();
            requests.flush_timers(&mut self.host.timers());
            (repaint, cursor)
        };
        if let Some(cursor) = cursor {
            self.cell.cursor.set(cursor);
        }
        self.host.set_cursor(self.cell.cursor.get());
        if repaint || self.cell.dirty.take() {
            self.host.invalidate();
            if self.access.active.get() {
                self.access.publish(&self.cell, &self.env.text);
            }
        }
        events
    }
}

#[cfg(test)]
mod tests;
