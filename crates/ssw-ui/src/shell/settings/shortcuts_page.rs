//! The Shortcuts page (spec §10, §11): every command with its bindings, rebinding by capturing
//! a key chord, conflict flags, the AltGr rule and reset to the defaults.
//!
//! The page edits nothing itself: it reports [`Edit`]s that the Settings window applies to
//! `settings.shortcuts`, then hands the new [`Keymap`] back through [`ShortcutsPage::set_keymap`].

use ssw_core::command::CommandId;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::keymap::{KeyChord, Keymap};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{Point, Rect, Size};
use crate::input::{Event, KeyEvent, PointerEventKind, vk};
use crate::shortcuts::{CharProbe, ShortcutRouter};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::list::{ListEvent, ListView, SelectionMode};
use crate::widgets::textbox::TextBox;
use crate::widgets::{CONTROL_HEIGHT, Control, FONT_SIZE, SharedClipboard, ui_style};

use super::form::{PAD, forward};

/// Gap between the filter, the list, the status line and the buttons.
const SECTION_GAP: f32 = 8.0;
/// Height of the status line.
const STATUS_HEIGHT: f32 = 40.0;
/// Gap between the buttons.
const BUTTON_GAP: f32 = 8.0;
/// Left and right Alt, left and right Shift/Ctrl, left and right Windows key.
const MODIFIER_KEYS: &[u16] = &[
    vk::SHIFT,
    vk::CONTROL,
    vk::MENU,
    0xA0,
    0xA1,
    0xA2,
    0xA3,
    0xA4,
    0xA5,
    0x5B,
    0x5C,
];

/// Names a key chord for display. The real implementation asks the OS (`GetKeyNameTextW`) so
/// keys carry the names of the user's keyboard layout.
pub trait KeyNames {
    /// The text shown for `chord`, for example `Ctrl+Shift+P`.
    fn chord_text(&self, chord: KeyChord) -> String;
}

/// Shows chords in their canonical English form (`Ctrl+Shift+P`).
#[derive(Debug, Clone, Copy, Default)]
pub struct DisplayKeyNames;

impl KeyNames for DisplayKeyNames {
    fn chord_text(&self, chord: KeyChord) -> String {
        chord.to_string()
    }
}

/// What the page needs from the outside at every call.
#[derive(Clone, Copy)]
pub(crate) struct Env<'a> {
    pub loc: &'a Localizer,
    pub names: &'a dyn KeyNames,
    pub probe: &'a dyn CharProbe,
}

/// A change of the bindings requested by the user.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Edit {
    /// The command gets exactly these chords (none unbinds it).
    Set(CommandId, Vec<KeyChord>),
    /// The command goes back to its default chords.
    Reset(CommandId),
    /// Every command goes back to its defaults.
    ResetAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Capture {
    command: CommandId,
    /// Add the chord to the existing ones instead of replacing them.
    add: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    /// Waiting for a key press.
    Prompt,
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
struct Status {
    tone: Tone,
    text: String,
}

/// The page: a filter box, the command list, a status line and the action buttons.
#[derive(Debug)]
pub(crate) struct ShortcutsPage {
    filter: TextBox,
    list: ListView,
    change: Button,
    add: Button,
    clear: Button,
    reset: Button,
    reset_all: Button,
    status_id: WidgetId,
    /// Every command with its label.
    commands: Vec<(CommandId, String)>,
    /// The commands the list shows, in list order.
    shown: Vec<CommandId>,
    selected: Option<CommandId>,
    keymap: Keymap,
    has_overrides: bool,
    capture: Option<Capture>,
    status: Option<Status>,
    edits: Vec<Edit>,
    bounds: Rect,
    status_rect: Rect,
}

impl ShortcutsPage {
    pub(crate) fn new(
        env: &Env,
        keymap: Keymap,
        has_overrides: bool,
        motion: Motion,
        clipboard: SharedClipboard,
    ) -> Self {
        let loc = env.loc;
        let commands = CommandId::all()
            .map(|command| (command, loc.text(command.label_id())))
            .collect();
        let button = |id: &str| Button::new(ButtonKind::Standard, loc.text(id)).with_motion(motion);
        let mut page = Self {
            filter: TextBox::new(loc.text("settings-shortcut-filter"), "", clipboard)
                .with_placeholder(loc.text("settings-shortcut-filter"))
                .with_motion(motion),
            list: ListView::new(
                loc.text("settings-shortcut-list"),
                Vec::new(),
                SelectionMode::Single,
            )
            .with_motion(motion),
            change: button("settings-shortcut-change"),
            add: button("settings-shortcut-add"),
            clear: button("settings-shortcut-clear"),
            reset: button("settings-shortcut-reset"),
            reset_all: button("settings-shortcut-reset-all"),
            status_id: WidgetId::allocate(),
            commands,
            shown: Vec::new(),
            selected: None,
            keymap,
            has_overrides,
            capture: None,
            status: None,
            edits: Vec::new(),
            bounds: Rect::ZERO,
            status_rect: Rect::ZERO,
        };
        page.refilter(env);
        page.update_buttons();
        page
    }

    /// True while the page waits for the user to press the new shortcut. The host must then
    /// deliver every key (Tab included) to the Settings window and skip its own shortcut
    /// routing, or the chord would run its command instead of being captured.
    pub(crate) fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    /// The command being rebound.
    pub(crate) fn capturing_command(&self) -> Option<CommandId> {
        self.capture.map(|capture| capture.command)
    }

    /// The text of the status line.
    pub(crate) fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|status| status.text.as_str())
    }

    /// The edits requested since the last call, oldest first.
    pub(crate) fn take_edits(&mut self) -> Vec<Edit> {
        std::mem::take(&mut self.edits)
    }

    /// Shows a new keymap (after the window applied an edit or the settings were replaced).
    pub(crate) fn set_keymap(&mut self, env: &Env, keymap: Keymap, has_overrides: bool) {
        self.keymap = keymap;
        self.has_overrides = has_overrides;
        self.refilter(env);
        self.update_buttons();
    }

    fn buttons(&self) -> [&Button; 5] {
        [
            &self.change,
            &self.add,
            &self.clear,
            &self.reset,
            &self.reset_all,
        ]
    }

    fn buttons_mut(&mut self) -> [&mut Button; 5] {
        [
            &mut self.change,
            &mut self.add,
            &mut self.clear,
            &mut self.reset,
            &mut self.reset_all,
        ]
    }

    /// Commands that share a chord with `command`.
    fn conflicting(&self, command: CommandId) -> Vec<CommandId> {
        let mut others = Vec::new();
        for chord in self.keymap.chords_for(command) {
            for other in self.keymap.conflicts_with(command, chord) {
                if !others.contains(&other) {
                    others.push(other);
                }
            }
        }
        others
    }

    fn label(&self, env: &Env, command: CommandId) -> String {
        env.loc.text(command.label_id())
    }

    fn labels(&self, env: &Env, commands: &[CommandId]) -> String {
        commands
            .iter()
            .map(|command| self.label(env, *command))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn chords_text(&self, env: &Env, command: CommandId) -> String {
        let chords: Vec<String> = self
            .keymap
            .chords_for(command)
            .map(|chord| env.names.chord_text(chord))
            .collect();
        if chords.is_empty() {
            env.loc.text("settings-shortcut-none")
        } else {
            chords.join(", ")
        }
    }

    /// The list row of `command`: its label, its chords and, when a chord is shared, the
    /// other commands using it.
    pub(crate) fn row_text(&self, env: &Env, command: CommandId) -> String {
        let label = self.label(env, command);
        let chords = self.chords_text(env, command);
        let others = self.conflicting(command);
        if others.is_empty() {
            env.loc.format(
                "settings-shortcut-row",
                &[("command", Arg::Str(&label)), ("chords", Arg::Str(&chords))],
            )
        } else {
            let others = self.labels(env, &others);
            env.loc.format(
                "settings-shortcut-row-conflict",
                &[
                    ("command", Arg::Str(&label)),
                    ("chords", Arg::Str(&chords)),
                    ("others", Arg::Str(&others)),
                ],
            )
        }
    }

    /// Rebuilds the list for the current filter and keymap, keeping the selection when the
    /// command is still shown.
    fn refilter(&mut self, env: &Env) {
        let query = self.filter.text().trim().to_lowercase();
        let keep = self.selected;
        let shown: Vec<CommandId> = self
            .commands
            .iter()
            .map(|(command, _)| *command)
            .filter(|command| {
                query.is_empty() || self.row_text(env, *command).to_lowercase().contains(&query)
            })
            .collect();
        let items = shown
            .iter()
            .map(|command| self.row_text(env, *command))
            .collect();
        self.shown = shown;
        self.list.set_items(items);
        self.selected = keep.filter(|command| self.shown.contains(command));
        if let Some(index) = self
            .selected
            .and_then(|command| self.shown.iter().position(|shown| *shown == command))
        {
            self.list.select(index);
        }
    }

    fn update_buttons(&mut self) {
        let has_selection = self.selected.is_some();
        self.change.set_enabled(has_selection);
        self.add.set_enabled(has_selection);
        self.clear.set_enabled(has_selection);
        self.reset.set_enabled(has_selection);
        self.reset_all.set_enabled(self.has_overrides);
    }

    /// The selected command. The buttons that need one are disabled without a selection, so they
    /// only report while there is one.
    fn selection(&self) -> CommandId {
        self.selected
            .expect("the capture buttons are enabled only with a selection")
    }

    fn start_capture(&mut self, cx: &mut EventCx, command: CommandId, add: bool, env: &Env) {
        self.capture = Some(Capture { command, add });
        let label = self.label(env, command);
        self.status = Some(Status {
            tone: Tone::Prompt,
            text: env.loc.format(
                "settings-shortcut-capture",
                &[("command", Arg::Str(&label))],
            ),
        });
        cx.invalidate(self.bounds);
    }

    fn end_capture(&mut self, cx: &mut EventCx) {
        self.capture = None;
        self.status = None;
        cx.invalidate(self.bounds);
    }

    /// Takes the key press while capturing: Esc cancels, modifiers alone are ignored,
    /// Ctrl+Alt chords the layout turns into text are refused (spec §11), anything else is
    /// the new shortcut.
    fn capture_key(
        &mut self,
        cx: &mut EventCx,
        capture: Capture,
        key: &KeyEvent,
        env: &Env,
    ) -> EventResult {
        if !key.is_down || MODIFIER_KEYS.contains(&key.vk) {
            return EventResult::handled();
        }
        if key.vk == vk::ESCAPE && key.mods.is_empty() {
            self.end_capture(cx);
            return EventResult::handled();
        }
        let chord = ShortcutRouter::chord_of(key);
        let chord_text = env.names.chord_text(chord);
        let types_text = chord.is_ctrl_alt()
            && [false, true].into_iter().any(|caps| {
                env.probe
                    .produces_char(chord.vk, key.scan, chord.shift, caps)
            });
        if types_text {
            self.status = Some(Status {
                tone: Tone::Error,
                text: env.loc.format(
                    "settings-shortcut-altgr",
                    &[("chord", Arg::Str(&chord_text))],
                ),
            });
            cx.invalidate(self.bounds);
            return EventResult::handled();
        }
        let mut chords: Vec<KeyChord> = if capture.add {
            self.keymap.chords_for(capture.command).collect()
        } else {
            Vec::new()
        };
        if !chords.contains(&chord) {
            chords.push(chord);
        }
        let others = self.keymap.conflicts_with(capture.command, chord);
        self.status = Some(if others.is_empty() {
            let label = self.label(env, capture.command);
            let all: Vec<String> = chords
                .iter()
                .map(|chord| env.names.chord_text(*chord))
                .collect();
            Status {
                tone: Tone::Info,
                text: env.loc.format(
                    "settings-shortcut-assigned",
                    &[
                        ("command", Arg::Str(&label)),
                        ("chords", Arg::Str(&all.join(", "))),
                    ],
                ),
            }
        } else {
            Status {
                tone: Tone::Warning,
                text: env.loc.format(
                    "settings-shortcut-conflict",
                    &[
                        ("chord", Arg::Str(&chord_text)),
                        ("others", Arg::Str(&self.labels(env, &others))),
                    ],
                ),
            }
        });
        self.edits.push(Edit::Set(capture.command, chords));
        self.capture = None;
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    /// Lays the page out in `bounds`: filter on top, buttons at the bottom, the status line and
    /// the list in between.
    pub(crate) fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let x = bounds.x + PAD;
        let width = (bounds.width - 2.0 * PAD).max(0.0);
        let filter_rect = Rect::new(x, bounds.y, width, CONTROL_HEIGHT);
        let buttons_y = bounds.bottom() - PAD / 2.0 - CONTROL_HEIGHT;
        self.status_rect = Rect::new(
            x,
            buttons_y - SECTION_GAP - STATUS_HEIGHT,
            width,
            STATUS_HEIGHT,
        );
        let list_top = filter_rect.bottom() + SECTION_GAP;
        let list_rect = Rect::new(
            x,
            list_top,
            width,
            (self.status_rect.y - SECTION_GAP - list_top).max(0.0),
        );
        let filter_id = self.filter.id();
        self.filter
            .arrange(&mut cx.for_widget(filter_id), filter_rect);
        let list_id = self.list.id();
        self.list.arrange(&mut cx.for_widget(list_id), list_rect);
        let mut left = x;
        for button in self.buttons_mut() {
            let id = button.id();
            let size = button.measure(&mut cx.for_widget(id), Size::UNBOUNDED);
            button.arrange(
                &mut cx.for_widget(id),
                Rect::new(left, buttons_y, size.width, CONTROL_HEIGHT),
            );
            left += size.width + BUTTON_GAP;
        }
    }

    pub(crate) fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        self.filter.paint(&cx.for_widget(self.filter.id()), canvas);
        self.list.paint(&cx.for_widget(self.list.id()), canvas);
        for button in self.buttons() {
            button.paint(&cx.for_widget(button.id()), canvas);
        }
        if let Some(status) = &self.status {
            let layout = cx.text.layout(
                &status.text,
                &ui_style(cx.theme, FONT_SIZE),
                &[],
                &[],
                self.status_rect.width,
            );
            let color = match status.tone {
                Tone::Prompt => tokens.accent,
                Tone::Info => tokens.text_muted,
                Tone::Warning => tokens.warning,
                Tone::Error => tokens.error,
            };
            canvas.draw_text_layout(
                layout.as_ref(),
                Point::new(self.status_rect.x, self.status_rect.y),
                color,
            );
        }
    }

    /// Routes `e`; while capturing, keys never reach the controls.
    pub(crate) fn event(&mut self, cx: &mut EventCx, e: &Event, env: &Env) -> EventResult {
        if let Some(capture) = self.capture {
            match e {
                Event::Key(key) => return self.capture_key(cx, capture, key, env),
                Event::Text(_) | Event::Ime(_) => return EventResult::handled(),
                Event::Pointer(pointer) if pointer.kind == PointerEventKind::Down => {
                    self.end_capture(cx);
                }
                _ => {}
            }
        }
        let mut result = forward(&mut self.filter, cx, e);
        result = result.or(forward(&mut self.list, cx, e));
        for button in self.buttons_mut() {
            result = result.or(forward(button, cx, e));
        }
        self.harvest(cx, env);
        result
    }

    /// Reacts to what the controls reported.
    fn harvest(&mut self, cx: &mut EventCx, env: &Env) {
        if self.filter.take_changed() {
            self.refilter(env);
        }
        for event in self.list.take_events() {
            match event {
                ListEvent::SelectionChanged => {
                    self.selected = self
                        .list
                        .selected_index()
                        .and_then(|index| self.shown.get(index).copied());
                }
                ListEvent::Activated(index) => {
                    let command = self.shown[index];
                    self.selected = Some(command);
                    self.start_capture(cx, command, false, env);
                }
            }
        }
        if self.change.take_activated() {
            self.start_capture(cx, self.selection(), false, env);
        }
        if self.add.take_activated() {
            self.start_capture(cx, self.selection(), true, env);
        }
        let selected = self.selected;
        if self.clear.take_activated()
            && let Some(command) = selected
        {
            self.status = None;
            self.edits.push(Edit::Set(command, Vec::new()));
        }
        if self.reset.take_activated()
            && let Some(command) = selected
        {
            self.status = None;
            self.edits.push(Edit::Reset(command));
        }
        if self.reset_all.take_activated() {
            self.status = None;
            self.edits.push(Edit::ResetAll);
        }
        self.update_buttons();
    }

    /// Describes the page's controls; returns their ids for the page node.
    pub(crate) fn access(&self, cx: &AccessCx, out: &mut AccessTree) -> Vec<WidgetId> {
        let mut ids = vec![self.filter.id(), self.list.id()];
        self.filter.access(&cx.for_widget(self.filter.id()), out);
        self.list.access(&cx.for_widget(self.list.id()), out);
        for button in self.buttons() {
            button.access(&cx.for_widget(button.id()), out);
            ids.push(button.id());
        }
        if let Some(status) = &self.status {
            out.insert(AccessNode::new(
                self.status_id,
                Role::Text,
                status.text.clone(),
                self.status_rect,
            ));
            ids.push(self.status_id);
        }
        ids
    }

    /// Carries out `a` on the control or list row with id `cx.id`.
    pub(crate) fn perform(&mut self, cx: &mut EventCx, a: &AccessAction, env: &Env) -> bool {
        let target = cx.id;
        let button = self
            .buttons()
            .iter()
            .position(|button| button.id() == target);
        let handled = if target == self.filter.id() {
            self.filter.perform(&mut cx.for_widget(target), a)
        } else if target == self.list.id() {
            self.list.perform(&mut cx.for_widget(target), a)
        } else if let Some(index) = button {
            let buttons = self.buttons_mut();
            buttons[index].perform(&mut cx.for_widget(target), a)
        } else {
            self.list
                .perform_item(&mut cx.for_widget(target), target, a)
        };
        self.harvest(cx, env);
        handled
    }
}
