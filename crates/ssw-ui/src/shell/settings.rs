//! The Settings window (spec §10): a navigation list of the eight pages and the page beside it.
//!
//! [`SettingsWindow`] is the root widget of the separate top-level Settings window. Every option
//! of `ssw_core::settings` has a control bound two ways to a [`Settings`] value the window owns:
//! changing a control writes the setting and emits [`SettingsEvent::Changed`]; a value that fails
//! validation (a number out of range, a malformed color, a file-name template without `{n}`…)
//! is refused, the setting keeps its previous value and the reason shows under the control.
//! The Shortcuts page lists every command with its bindings and rebinds by capturing a chord.
//!
//! The window never touches the OS. Folder pickers, opening the logs folder and the
//! confirmation of "reset settings" are [`SettingsEvent`]s the app answers.

pub(crate) mod fields;
pub(crate) mod form;
mod shortcuts_page;

use std::fmt;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::keymap::{KeyChord, Keymap, Overrides};
use ssw_core::settings::{Settings, SettingsSection};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash};
use crate::geometry::{Point, Rect, Size};
use crate::input::Event;
use crate::shortcuts::CharProbe;
use crate::text::FontWeight;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::combo::ComboBox;
use crate::widgets::list::{ListEvent, ListView, SelectionMode};
use crate::widgets::slider::Slider;
use crate::widgets::textbox::TextBox;
use crate::widgets::toggle::ToggleSwitch;
use crate::widgets::{Control, SharedClipboard, ui_style};

use fields::{FIELDS, FieldDef, FieldKind, FieldValue, display_number, is_enabled};
use form::{Change, Form, FormScroll, Input, PAD, Row, forward};
use shortcuts_page::{Edit, Env, ShortcutsPage};

pub use fields::FieldId;
pub use shortcuts_page::{DisplayKeyNames, KeyNames};

/// Width of the navigation column.
const NAV_WIDTH: f32 = 220.0;
/// Padding around the navigation list.
const NAV_PAD: f32 = 8.0;
/// Height of a navigation row.
const NAV_ROW: f32 = 40.0;
/// Height of the page title.
const HEADER_HEIGHT: f32 = 64.0;
/// Em size of the page title.
const TITLE_SIZE: f32 = 20.0;
/// Size the window asks for when its parent does not constrain it.
const DEFAULT_SIZE: Size = Size::new(900.0, 640.0);

/// A language the user can pick in General → Language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageChoice {
    /// BCP 47 tag stored in `general.language`.
    pub tag: String,
    /// The language's name in that language.
    pub name: String,
}

/// What the Settings window needs from the app.
pub struct SettingsEnv {
    /// The clipboard of the window's text boxes.
    pub clipboard: SharedClipboard,
    /// Asks the keyboard layout whether Ctrl+Alt+key types a character (the AltGr rule).
    pub char_probe: Box<dyn CharProbe>,
    /// Names chords for display.
    pub key_names: Box<dyn KeyNames>,
    /// The UI languages the app ships.
    pub languages: Vec<LanguageChoice>,
    /// The user's motion preference.
    pub motion: Motion,
}

impl fmt::Debug for SettingsEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsEnv")
            .field("languages", &self.languages)
            .field("motion", &self.motion)
            .finish_non_exhaustive()
    }
}

/// What changed in a [`SettingsEvent::Changed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsChange {
    /// The value of one option.
    Field(FieldId),
    /// The shortcut bindings.
    Shortcuts,
}

/// Something the app has to react to; drained with [`SettingsWindow::take_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsEvent {
    /// A setting changed: persist [`SettingsWindow::settings`] and apply it.
    Changed(SettingsChange),
    /// The Browse button of a folder option was pressed: show a folder picker and answer with
    /// [`SettingsWindow::set_folder`].
    BrowseFolder(FieldId),
    /// General → Default apps…: open the system page where the user picks the app that opens
    /// Markdown and text files (spec §12).
    OpenDefaultApps,
    /// Advanced → Open logs folder.
    OpenLogsFolder,
    /// Advanced → Open settings.json: open the file in the editor.
    OpenSettingsFile,
    /// Advanced → Reset settings: confirm with the user, then replace the settings with
    /// [`SettingsWindow::set_settings`].
    ResetSettings,
}

/// Identifies a row of a settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKey {
    Field(FieldId),
    DefaultApps,
    OpenLogs,
    OpenFile,
    Reset,
}

/// The Settings window.
pub struct SettingsWindow {
    id: WidgetId,
    page_id: WidgetId,
    loc: Rc<Localizer>,
    env: SettingsEnv,
    settings: Settings,
    /// The options of the language drop-down as `(tag, name)`; the first is "system default".
    language_options: Vec<(String, String)>,
    nav: ListView,
    section: SettingsSection,
    forms: Vec<(SettingsSection, Form<RowKey>)>,
    shortcuts: ShortcutsPage,
    /// Scrolls the page of the current section.
    scroll: FormScroll,
    bounds: Rect,
    nav_rect: Rect,
    page_rect: Rect,
    events: Vec<SettingsEvent>,
}

impl fmt::Debug for SettingsWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsWindow")
            .field("id", &self.id)
            .field("section", &self.section)
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

fn shortcut_env<'a>(loc: &'a Rc<Localizer>, env: &'a SettingsEnv) -> Env<'a> {
    Env {
        loc,
        names: env.key_names.as_ref(),
        probe: env.char_probe.as_ref(),
    }
}

/// The language drop-down: system default, the shipped languages and, when the stored tag is
/// none of them, the stored tag itself so it is not lost.
fn language_options(
    loc: &Localizer,
    env: &SettingsEnv,
    settings: &Settings,
) -> Vec<(String, String)> {
    let mut options = vec![(String::new(), loc.text("settings-opt-language-system"))];
    options.extend(
        env.languages
            .iter()
            .map(|language| (language.tag.clone(), language.name.clone())),
    );
    if let Some(tag) = &settings.general.language
        && !options.iter().any(|(known, _)| known == tag)
    {
        options.push((tag.clone(), tag.clone()));
    }
    options
}

fn field_row(
    loc: &Localizer,
    env: &SettingsEnv,
    settings: &Settings,
    languages: &[(String, String)],
    def: &FieldDef,
) -> Row<RowKey> {
    let label = loc.text(def.label);
    let hint = def.hint.map(|id| loc.text(id));
    let value = def.read(settings);
    let motion = env.motion;
    let text_box = |text: String| {
        TextBox::new(label.clone(), text, Rc::clone(&env.clipboard)).with_motion(motion)
    };
    let input = match def.kind {
        FieldKind::Toggle => {
            Input::Toggle(ToggleSwitch::new(label.clone(), value.flag()).with_motion(motion))
        }
        FieldKind::Choice(options) => {
            let items = options.iter().map(|id| loc.text(id)).collect();
            Input::Choice(Box::new(
                ComboBox::new(label.clone(), items, Some(value.index())).with_motion(motion),
            ))
        }
        FieldKind::Language => {
            let items = languages.iter().map(|(_, name)| name.clone()).collect();
            let index = languages
                .iter()
                .position(|(tag, _)| tag == value.text())
                .unwrap_or(0);
            Input::Choice(Box::new(
                ComboBox::new(label.clone(), items, Some(index)).with_motion(motion),
            ))
        }
        FieldKind::Text | FieldKind::Folder => Input::Text(text_box(value.text().to_owned())),
        FieldKind::Number { .. } => Input::Text(text_box(display_number(value.number()))),
        FieldKind::Percent { max } => {
            let labels = (0..=i64::from(max))
                .map(|n| loc.format("settings-percent", &[("value", Arg::Int(n))]))
                .collect();
            let slider = Slider::new(label.clone(), 0.0, f64::from(max), value.number(), 1.0)
                .with_large_step(10.0)
                .with_motion(motion);
            Input::Slider { slider, labels }
        }
    };
    let row = Row::new(RowKey::Field(def.id), label.clone(), hint, input);
    if matches!(def.kind, FieldKind::Folder) {
        row.with_accessory(
            Button::new(ButtonKind::Standard, loc.text("settings-browse")).with_motion(motion),
        )
    } else {
        row
    }
}

fn action_row(loc: &Localizer, motion: Motion, key: RowKey, id: &str) -> Row<RowKey> {
    let hint_id = format!("{id}-hint");
    let hint = loc.has_message(&hint_id).then(|| loc.text(&hint_id));
    let button =
        Button::new(ButtonKind::Standard, loc.text(&format!("{id}-button"))).with_motion(motion);
    Row::new(key, loc.text(id), hint, Input::Button(button))
}

fn build_form(
    loc: &Localizer,
    env: &SettingsEnv,
    settings: &Settings,
    languages: &[(String, String)],
    section: SettingsSection,
) -> Form<RowKey> {
    let mut rows: Vec<Row<RowKey>> = FIELDS
        .iter()
        .filter(|def| def.section == section)
        .map(|def| field_row(loc, env, settings, languages, def))
        .collect();
    if section == SettingsSection::General {
        rows.push(action_row(
            loc,
            env.motion,
            RowKey::DefaultApps,
            "settings-general-default-apps",
        ));
    }
    if section == SettingsSection::Advanced {
        rows.push(action_row(
            loc,
            env.motion,
            RowKey::OpenLogs,
            "settings-advanced-open-logs",
        ));
        rows.push(action_row(
            loc,
            env.motion,
            RowKey::OpenFile,
            "settings-advanced-open-file",
        ));
        rows.push(action_row(
            loc,
            env.motion,
            RowKey::Reset,
            "settings-advanced-reset",
        ));
    }
    Form::new(rows)
}

/// The form of `section` (borrowing only the forms, so the caller can use its other fields).
fn find_form(
    forms: &[(SettingsSection, Form<RowKey>)],
    section: SettingsSection,
) -> Option<&Form<RowKey>> {
    forms
        .iter()
        .find(|(candidate, _)| *candidate == section)
        .map(|(_, form)| form)
}

/// The form of `section`, mutably.
fn find_form_mut(
    forms: &mut [(SettingsSection, Form<RowKey>)],
    section: SettingsSection,
) -> Option<&mut Form<RowKey>> {
    forms
        .iter_mut()
        .find(|(candidate, _)| *candidate == section)
        .map(|(_, form)| form)
}

/// The form of a section whose page is a form: every section but Shortcuts, which is also the
/// only section no field belongs to.
fn page_form(forms: &[(SettingsSection, Form<RowKey>)], section: SettingsSection) -> &Form<RowKey> {
    find_form(forms, section).expect("every page but Shortcuts is a form")
}

/// [`page_form`], mutably.
fn page_form_mut(
    forms: &mut [(SettingsSection, Form<RowKey>)],
    section: SettingsSection,
) -> &mut Form<RowKey> {
    find_form_mut(forms, section).expect("every page but Shortcuts is a form")
}

/// Records `chords` as the override of `command`, or drops the override when they are the
/// command's defaults.
fn set_override(overrides: &mut Overrides, command: CommandId, chords: Vec<KeyChord>) {
    let mut wanted = chords.clone();
    wanted.sort_unstable();
    let mut defaults: Vec<KeyChord> = Keymap::default_chords(command).collect();
    defaults.sort_unstable();
    if wanted == defaults {
        overrides.remove(command.as_str());
    } else {
        overrides.insert(command.as_str().to_owned(), chords);
    }
}

impl SettingsWindow {
    /// A window showing `settings`, with the General page selected.
    #[must_use]
    pub fn new(settings: Settings, loc: Rc<Localizer>, env: SettingsEnv) -> Self {
        let shortcuts = ShortcutsPage::new(
            &shortcut_env(&loc, &env),
            settings.keymap(),
            !settings.shortcuts.is_empty(),
            env.motion,
            Rc::clone(&env.clipboard),
        );
        let mut window = Self {
            id: WidgetId::allocate(),
            page_id: WidgetId::allocate(),
            nav: Self::build_nav(&loc, env.motion),
            scroll: FormScroll::new(loc.text("settings-scroll"), env.motion),
            loc,
            env,
            settings,
            language_options: Vec::new(),
            section: SettingsSection::General,
            forms: Vec::new(),
            shortcuts,
            bounds: Rect::ZERO,
            nav_rect: Rect::ZERO,
            page_rect: Rect::ZERO,
            events: Vec::new(),
        };
        window.rebuild_forms();
        window.nav.select(0);
        window
    }

    fn build_nav(loc: &Localizer, motion: Motion) -> ListView {
        ListView::new(
            loc.text("settings-nav"),
            SettingsSection::ALL
                .iter()
                .map(|section| loc.text(section.label_id()))
                .collect(),
            SelectionMode::Single,
        )
        .with_row_height(NAV_ROW)
        .with_motion(motion)
    }

    /// Rebuilds the forms from the settings (the controls start from the stored values).
    fn rebuild_forms(&mut self) {
        self.language_options = language_options(&self.loc, &self.env, &self.settings);
        self.forms = SettingsSection::ALL
            .iter()
            .copied()
            .filter(|section| *section != SettingsSection::Shortcuts)
            .map(|section| {
                let form = build_form(
                    &self.loc,
                    &self.env,
                    &self.settings,
                    &self.language_options,
                    section,
                );
                (section, form)
            })
            .collect();
        self.refresh_enabled();
    }

    /// Enables the controls that depend on another option (the autosave delay needs autosave,
    /// the custom accent needs the custom mode, …).
    fn refresh_enabled(&mut self) {
        for def in FIELDS {
            let enabled = is_enabled(def.id, &self.settings);
            page_form_mut(&mut self.forms, def.section).set_enabled(RowKey::Field(def.id), enabled);
        }
    }

    /// The settings as edited so far.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Replaces the settings (after a reset or an external edit of `settings.json`); every
    /// control shows the new values.
    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
        self.rebuild_forms();
        let env = shortcut_env(&self.loc, &self.env);
        self.shortcuts.set_keymap(
            &env,
            self.settings.keymap(),
            !self.settings.shortcuts.is_empty(),
        );
    }

    /// Shows every text in the language of `loc` (after the user picked another language).
    pub fn relocalize(&mut self, loc: Rc<Localizer>) {
        self.loc = loc;
        self.nav = Self::build_nav(&self.loc, self.env.motion);
        self.nav.select(self.section_index());
        self.scroll = FormScroll::new(self.loc.text("settings-scroll"), self.env.motion);
        self.shortcuts = ShortcutsPage::new(
            &shortcut_env(&self.loc, &self.env),
            self.settings.keymap(),
            !self.settings.shortcuts.is_empty(),
            self.env.motion,
            Rc::clone(&self.env.clipboard),
        );
        self.rebuild_forms();
    }

    /// The page being shown.
    #[must_use]
    pub fn section(&self) -> SettingsSection {
        self.section
    }

    fn section_index(&self) -> usize {
        SettingsSection::ALL
            .iter()
            .position(|section| *section == self.section)
            .unwrap_or(0)
    }

    /// Shows `section`.
    pub fn select_section(&mut self, section: SettingsSection) {
        self.show_section(section);
        self.nav.select(self.section_index());
    }

    fn show_section(&mut self, section: SettingsSection) {
        if section != self.section {
            self.section = section;
            self.scroll.reset();
        }
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<SettingsEvent> {
        std::mem::take(&mut self.events)
    }

    /// The message under the control of `field` while its typed value is refused.
    #[must_use]
    pub fn error(&self, field: FieldId) -> Option<&str> {
        page_form(&self.forms, field.section()).error(RowKey::Field(field))
    }

    /// Answers [`SettingsEvent::BrowseFolder`]: stores the chosen folder like a typed value.
    pub fn set_folder(&mut self, field: FieldId, path: &str) {
        page_form_mut(&mut self.forms, field.section()).set_text(RowKey::Field(field), path);
        self.commit(field, Change::Edited(path.to_owned()));
    }

    /// True while the Shortcuts page waits for the new chord. The host must then deliver every
    /// key press to this window, Tab included, and skip its own shortcut routing.
    #[must_use]
    pub fn is_capturing_shortcut(&self) -> bool {
        self.shortcuts.is_capturing()
    }

    /// The command being rebound.
    #[must_use]
    pub fn capturing_command(&self) -> Option<CommandId> {
        self.shortcuts.capturing_command()
    }

    /// The Shortcuts page row of `command`: its name, its chords and, when a chord is shared,
    /// the commands it conflicts with.
    #[must_use]
    pub fn shortcut_label(&self, command: CommandId) -> String {
        self.shortcuts
            .row_text(&shortcut_env(&self.loc, &self.env), command)
    }

    /// The status line of the Shortcuts page (the capture prompt, a conflict or AltGr warning).
    #[must_use]
    pub fn shortcut_status(&self) -> Option<&str> {
        self.shortcuts.status_text()
    }

    /// Writes a change reported by a control into the settings, or shows why it was refused.
    fn commit(&mut self, field: FieldId, change: Change) {
        let def = field.def();
        let value = match change {
            Change::Toggled(on) => Ok(FieldValue::Bool(on)),
            Change::Selected(index) => Ok(if matches!(def.kind, FieldKind::Language) {
                let tag = self
                    .language_options
                    .get(index)
                    .map(|(tag, _)| tag.clone())
                    .unwrap_or_default();
                FieldValue::Text(tag)
            } else {
                FieldValue::Choice(index)
            }),
            Change::Edited(text) => def.parse_input(&text),
            Change::Slid(number) => Ok(FieldValue::Number(number)),
            Change::Accessory => {
                self.events.push(SettingsEvent::BrowseFolder(field));
                return;
            }
            Change::Submitted | Change::Pressed => return,
        };
        let outcome = value.and_then(|value| def.write(&mut self.settings, &value));
        let message = outcome
            .as_ref()
            .err()
            .map(|problem| problem.message(&self.loc));
        page_form_mut(&mut self.forms, def.section).set_error(RowKey::Field(field), message);
        if outcome.is_ok() {
            self.events
                .push(SettingsEvent::Changed(SettingsChange::Field(field)));
            self.refresh_enabled();
        }
    }

    /// Applies what the controls reported since the last call.
    fn harvest(&mut self, cx: &mut EventCx) {
        let selected = self
            .nav
            .take_events()
            .contains(&ListEvent::SelectionChanged);
        let section = self
            .nav
            .selected_index()
            .and_then(|index| SettingsSection::ALL.get(index).copied());
        if selected && let Some(section) = section {
            self.show_section(section);
            cx.invalidate_all();
        }
        let mut changes = Vec::new();
        for (_, form) in &mut self.forms {
            changes.extend(form.take_changes());
        }
        for (key, change) in changes {
            match key {
                RowKey::DefaultApps => self.events.push(SettingsEvent::OpenDefaultApps),
                RowKey::OpenLogs => self.events.push(SettingsEvent::OpenLogsFolder),
                RowKey::OpenFile => self.events.push(SettingsEvent::OpenSettingsFile),
                RowKey::Reset => self.events.push(SettingsEvent::ResetSettings),
                RowKey::Field(field) => self.commit(field, change),
            }
        }
        self.apply_shortcut_edits();
        self.reveal(cx);
    }

    fn apply_shortcut_edits(&mut self) {
        let edits = self.shortcuts.take_edits();
        if edits.is_empty() {
            return;
        }
        for edit in edits {
            match edit {
                Edit::Set(command, chords) => {
                    set_override(&mut self.settings.shortcuts, command, chords);
                }
                Edit::Reset(command) => {
                    self.settings.shortcuts.remove(command.as_str());
                }
                Edit::ResetAll => {
                    // Ids this version does not know belong to a newer one and are kept.
                    self.settings
                        .shortcuts
                        .retain(|id, _| id.parse::<CommandId>().is_err());
                }
            }
        }
        let env = shortcut_env(&self.loc, &self.env);
        self.shortcuts.set_keymap(
            &env,
            self.settings.keymap(),
            !self.settings.shortcuts.is_empty(),
        );
        self.events
            .push(SettingsEvent::Changed(SettingsChange::Shortcuts));
    }

    /// Scrolls the page so the focused control and an open drop-down are visible, once per
    /// change of focus or popup (so the user's own scrolling is never undone).
    fn reveal(&mut self, cx: &mut EventCx) {
        if let Some(form) = find_form(&self.forms, self.section) {
            self.scroll.reveal(cx, form);
        }
    }
}

impl Control for SettingsWindow {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for SettingsWindow {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        Size::new(
            if available.width.is_finite() {
                available.width
            } else {
                DEFAULT_SIZE.width
            },
            if available.height.is_finite() {
                available.height
            } else {
                DEFAULT_SIZE.height
            },
        )
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        self.nav_rect = Rect::new(
            bounds.x,
            bounds.y,
            NAV_WIDTH.min(bounds.width),
            bounds.height,
        );
        self.page_rect = Rect::new(
            self.nav_rect.right(),
            bounds.y,
            (bounds.width - self.nav_rect.width).max(0.0),
            bounds.height,
        );
        let viewport = Rect::new(
            self.page_rect.x,
            self.page_rect.y + HEADER_HEIGHT,
            self.page_rect.width,
            (self.page_rect.height - HEADER_HEIGHT).max(0.0),
        );
        let nav_id = self.nav.id();
        let nav_bounds = Rect::new(
            self.nav_rect.x + NAV_PAD,
            self.nav_rect.y + NAV_PAD,
            (self.nav_rect.width - 2.0 * NAV_PAD).max(0.0),
            (self.nav_rect.height - 2.0 * NAV_PAD).max(0.0),
        );
        self.nav.arrange(&mut cx.for_widget(nav_id), nav_bounds);
        let section = self.section;
        if section == SettingsSection::Shortcuts {
            self.shortcuts.arrange(cx, viewport);
            self.scroll.arrange(cx, viewport, 0.0);
        } else {
            let form = page_form_mut(&mut self.forms, section);
            let content = form.layout(cx, viewport);
            self.scroll.arrange(cx, viewport, content);
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        canvas.fill_rect(self.nav_rect, tokens.sidebar_bg);
        canvas.fill_rect(self.page_rect, tokens.surface);
        canvas.stroke_line(
            Point::new(self.nav_rect.right() - 0.5, self.nav_rect.y),
            Point::new(self.nav_rect.right() - 0.5, self.nav_rect.bottom()),
            tokens.hairline,
            1.0,
            Dash::Solid,
        );
        self.nav.paint(&cx.for_widget(self.nav.id()), canvas);
        let title = cx.text.layout(
            &self.loc.text(self.section.label_id()),
            &ui_style(cx.theme, TITLE_SIZE).with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        );
        canvas.draw_text_layout(
            title.as_ref(),
            Point::new(
                self.page_rect.x + PAD,
                self.page_rect.y + (HEADER_HEIGHT - title.size().height) / 2.0,
            ),
            tokens.text,
        );
        if self.section == SettingsSection::Shortcuts {
            self.shortcuts.paint(cx, canvas);
            return;
        }
        self.scroll
            .paint(cx, canvas, page_form(&self.forms, self.section));
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let capturing = self.shortcuts.is_capturing()
            && matches!(e, Event::Key(_) | Event::Text(_) | Event::Ime(_));
        let mut result = if self.section == SettingsSection::Shortcuts {
            let env = shortcut_env(&self.loc, &self.env);
            self.shortcuts.event(cx, e, &env)
        } else {
            self.route_to_form(cx, e)
        };
        if !capturing {
            result = result.or(forward(&mut self.nav, cx, e));
        }
        self.harvest(cx);
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.nav.access(&cx.for_widget(self.nav.id()), out);
        let children = if self.section == SettingsSection::Shortcuts {
            self.shortcuts.access(cx, out)
        } else {
            self.scroll
                .access(cx, out, page_form(&self.forms, self.section))
        };
        out.insert(
            AccessNode::new(
                self.page_id,
                Role::Group,
                self.loc.text(self.section.label_id()),
                self.page_rect,
            )
            .with_children(children),
        );
        out.insert(
            AccessNode::new(
                self.id,
                Role::Window,
                self.loc.text("settings-title"),
                self.bounds,
            )
            .with_children([self.nav.id(), self.page_id]),
        );
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        let target = cx.id;
        let mut handled = self.nav.perform_item(&mut cx.for_widget(target), target, a);
        if !handled && target == self.nav.id() {
            handled = self.nav.perform(&mut cx.for_widget(target), a);
        }
        if !handled {
            handled = if self.section == SettingsSection::Shortcuts {
                let env = shortcut_env(&self.loc, &self.env);
                self.shortcuts.perform(cx, a, &env)
            } else {
                find_form_mut(&mut self.forms, self.section)
                    .is_some_and(|form| self.scroll.perform(cx, form, a))
            };
        }
        self.harvest(cx);
        handled
    }
}

impl SettingsWindow {
    /// Routes `e` to the scroll bar and the controls of the current page.
    fn route_to_form(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        find_form_mut(&mut self.forms, self.section)
            .map_or_else(EventResult::ignored, |form| self.scroll.event(cx, form, e))
    }
}
