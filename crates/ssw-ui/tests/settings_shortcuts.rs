//! The Shortcuts page: every command with its binding, rebinding by capturing a chord, conflict
//! flags, the AltGr rule (spec §11) and reset to the defaults.

mod settings_support;

use settings_support::{Host, localizer, settings_window};
use ssw_core::command::CommandId;
use ssw_core::keymap::{KeyChord, Keymap, vk as key};
use ssw_core::settings::{Settings, SettingsSection};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::input::{Event, Modifiers, PointerEvent, PointerEventKind, TextInput, vk};
use ssw_ui::shell::settings::{SettingsChange, SettingsEvent, SettingsWindow};

const WINDOW: Rect = Rect::new(0.0, 0.0, 900.0, 640.0);
const CHANGED: SettingsChange = SettingsChange::Shortcuts;

fn open(settings: Settings) -> (SettingsWindow, Host) {
    let mut window = settings_window(settings);
    let mut host = Host::new(WINDOW);
    window.select_section(SettingsSection::Shortcuts);
    host.layout(&mut window);
    (window, host)
}

/// Filters the list down to `filter`, then selects the row named `row`.
fn select(window: &mut SettingsWindow, host: &mut Host, filter: &str, row: &str) {
    let filter_box = host.node(window, Role::Edit, "Filter commands");
    assert!(host.perform(
        window,
        filter_box.id,
        AccessAction::SetValue(filter.to_owned())
    ));
    let item = host.node(window, Role::ListItem, row);
    assert!(host.perform(window, item.id, AccessAction::Select));
}

fn press_button(window: &mut SettingsWindow, host: &mut Host, name: &str) {
    let button = host.node(window, Role::Button, name);
    assert!(
        host.perform(window, button.id, AccessAction::Invoke),
        "{name} is not available"
    );
}

fn chord(vk: u16, ctrl: bool, shift: bool, alt: bool) -> KeyChord {
    let mut chord = KeyChord::new(vk);
    if ctrl {
        chord = chord.with_ctrl();
    }
    if shift {
        chord = chord.with_shift();
    }
    if alt {
        chord = chord.with_alt();
    }
    chord
}

fn changed_shortcuts() -> Vec<SettingsEvent> {
    vec![SettingsEvent::Changed(CHANGED)]
}

#[test]
fn every_command_is_listed_with_its_bindings() {
    let loc = localizer();
    let (window, _) = open(Settings::default());
    for command in CommandId::all() {
        let chords: Vec<String> = Keymap::default_chords(command)
            .map(|chord| chord.to_string())
            .collect();
        let chords = if chords.is_empty() {
            "Not assigned".to_owned()
        } else {
            chords.join(", ")
        };
        let expected = format!("{} — {chords}", loc.text(command.label_id()));
        assert_eq!(window.shortcut_label(command), expected, "{command}");
    }
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B"
    );
    assert!(
        window
            .shortcut_label(CommandId::TabNext)
            .contains("Ctrl+Tab, ")
    );
}

#[test]
fn the_list_shows_the_first_commands_and_filters_by_name_or_chord() {
    let (mut window, mut host) = open(Settings::default());
    let rows = |host: &Host, window: &SettingsWindow| -> Vec<String> {
        host.tree(window)
            .nodes()
            .iter()
            .filter(|n| n.role == Role::ListItem && n.name.contains(" — "))
            .map(|n| n.name.clone())
            .collect()
    };
    let first = rows(&host, &window);
    assert!(first.len() >= 8, "{first:?}");
    assert_eq!(
        first[0],
        window.shortcut_label(CommandId::all().next().unwrap_or(CommandId::FileNew))
    );
    let filter_box = host.node(&window, Role::Edit, "Filter commands");
    host.perform(
        &mut window,
        filter_box.id,
        AccessAction::SetValue("zoom".into()),
    );
    let zoom = rows(&host, &window);
    let expected: Vec<String> = CommandId::all()
        .map(|c| window.shortcut_label(c))
        .filter(|label| label.to_lowercase().contains("zoom"))
        .collect();
    assert_eq!(zoom, expected);
    assert!(zoom.len() >= 3);
    // The chord is searchable too.
    host.perform(
        &mut window,
        filter_box.id,
        AccessAction::SetValue("ctrl+shift+k".into()),
    );
    let by_chord = rows(&host, &window);
    assert_eq!(by_chord.len(), 1, "{by_chord:?}");
    assert!(by_chord[0].contains("Ctrl+Shift+K"));
}

#[test]
fn the_buttons_need_a_selection() {
    let (mut window, mut host) = open(Settings::default());
    for name in ["Change", "Add another", "Remove", "Reset", "Reset all"] {
        assert!(
            host.node(&window, Role::Button, name).state.disabled,
            "{name}"
        );
    }
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    for name in ["Change", "Add another", "Remove", "Reset"] {
        assert!(
            !host.node(&window, Role::Button, name).state.disabled,
            "{name}"
        );
    }
    // Nothing was overridden yet, so there is nothing to reset everywhere.
    assert!(host.node(&window, Role::Button, "Reset all").state.disabled);
}

#[test]
fn rebinding_captures_the_next_chord() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    press_button(&mut window, &mut host, "Change");
    assert!(window.is_capturing_shortcut());
    assert_eq!(window.capturing_command(), Some(CommandId::FormatBold));
    assert_eq!(
        window.shortcut_status(),
        Some("Press the new shortcut for Bold. Esc cancels.")
    );
    assert!(host.has_node(
        &window,
        Role::Text,
        "Press the new shortcut for Bold. Esc cancels."
    ));
    // A modifier on its own is not a chord.
    host.key(&mut window, vk::CONTROL, Modifiers::CTRL);
    assert!(window.is_capturing_shortcut());
    assert!(window.take_events().is_empty());
    // Keys never reach the controls while capturing: text is swallowed and Tab is a chord.
    let typed = host.event(&mut window, Event::Text(TextInput("x".into())));
    assert!(typed.handled);
    host.key(&mut window, key::B, Modifiers::CTRL | Modifiers::SHIFT);
    assert!(!window.is_capturing_shortcut());
    assert_eq!(window.take_events(), changed_shortcuts());
    let stored = window.settings().shortcuts.get("format.bold");
    assert_eq!(stored, Some(&vec![chord(key::B, true, true, false)]));
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+Shift+B"
    );
    assert_eq!(window.shortcut_status(), Some("Bold: Ctrl+Shift+B"));
    let keymap = window.settings().keymap();
    assert_eq!(
        keymap.lookup(chord(key::B, true, true, false)),
        Some(CommandId::FormatBold)
    );
    assert_eq!(keymap.lookup(chord(key::B, true, false, false)), None);
    // The new binding shows in the list row too.
    assert!(host.has_node(&window, Role::ListItem, "Bold — Ctrl+Shift+B"));
}

#[test]
fn adding_a_chord_keeps_the_existing_ones() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    press_button(&mut window, &mut host, "Add another");
    host.key(&mut window, key::B, Modifiers::CTRL | Modifiers::SHIFT);
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B, Ctrl+Shift+B"
    );
    // Adding a chord the command already has changes nothing, and returning to the defaults
    // drops the override entirely.
    press_button(&mut window, &mut host, "Change");
    host.key(&mut window, key::B, Modifiers::CTRL);
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B"
    );
    assert!(window.settings().shortcuts.is_empty());
}

#[test]
fn shared_chords_are_flagged_on_both_commands() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "italic", "Italic — Ctrl+I");
    press_button(&mut window, &mut host, "Change");
    host.key(&mut window, key::B, Modifiers::CTRL);
    assert_eq!(
        window.shortcut_status(),
        Some("Ctrl+B is also used by Bold. Neither command runs until one of them is changed.")
    );
    assert_eq!(
        window.shortcut_label(CommandId::FormatItalic),
        "Italic — Ctrl+B — conflicts with Bold"
    );
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B — conflicts with Italic"
    );
    let keymap = window.settings().keymap();
    assert_eq!(keymap.conflicts().len(), 1);
    assert_eq!(keymap.lookup(chord(key::B, true, false, false)), None);
    // The warning is drawn in the warning color.
    let canvas = host.paint(&window);
    let warning = host.theme.tokens.warning;
    assert!(canvas.ops().any(|op| matches!(
        op,
        ssw_ui::canvas::DrawOp::Text { text, color, .. }
            if text.starts_with("Ctrl+B is also used") && *color == warning
    )));
    // Giving Italic its own chord clears the flag.
    press_button(&mut window, &mut host, "Reset");
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B"
    );
    assert!(window.settings().keymap().conflicts().is_empty());
}

#[test]
fn ctrl_alt_chords_that_type_text_are_refused() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "italic", "Italic — Ctrl+I");
    press_button(&mut window, &mut host, "Change");
    for (vk, shift) in [(key::Q, false), (key::E, true), (key::A, false)] {
        let mods = if shift {
            Modifiers::CTRL | Modifiers::ALT | Modifiers::SHIFT
        } else {
            Modifiers::CTRL | Modifiers::ALT
        };
        host.key(&mut window, vk, mods);
        assert!(window.is_capturing_shortcut());
        let status = window.shortcut_status().unwrap_or_default().to_owned();
        assert!(
            status.contains("types a character on your keyboard layout"),
            "{status}"
        );
        assert!(window.take_events().is_empty());
        assert!(window.settings().shortcuts.is_empty());
    }
    assert_eq!(
        window.shortcut_status(),
        Some(
            "Ctrl+Alt+A types a character on your keyboard layout (AltGr), \
             so it cannot be a shortcut. Press another one."
        )
    );
    // Ctrl+Alt+W types nothing on this layout, so it is a valid shortcut.
    host.key(&mut window, key::W, Modifiers::CTRL | Modifiers::ALT);
    assert!(!window.is_capturing_shortcut());
    assert_eq!(
        window.settings().shortcuts.get("format.italic"),
        Some(&vec![chord(key::W, true, false, true)])
    );
    assert_eq!(window.take_events(), changed_shortcuts());
}

#[test]
fn escape_and_a_click_cancel_the_capture() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    press_button(&mut window, &mut host, "Change");
    host.key(&mut window, vk::ESCAPE, Modifiers::NONE);
    assert!(!window.is_capturing_shortcut());
    assert_eq!(window.shortcut_status(), None);
    assert!(window.take_events().is_empty());
    press_button(&mut window, &mut host, "Change");
    host.click(&mut window, Point::new(5.0, 5.0));
    assert!(!window.is_capturing_shortcut());
    assert!(window.settings().shortcuts.is_empty());
}

#[test]
fn enter_on_a_row_starts_the_capture() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    let list = host.node(&window, Role::List, "Commands and their shortcuts");
    assert!(host.perform(&mut window, list.id, AccessAction::Focus));
    host.key(&mut window, vk::RETURN, Modifiers::NONE);
    assert_eq!(window.capturing_command(), Some(CommandId::FormatBold));
    // The key release of that Enter is not captured as a chord.
    let release = ssw_ui::input::KeyEvent {
        is_down: false,
        ..ssw_ui::input::KeyEvent::press(vk::RETURN, Modifiers::NONE)
    };
    host.event(&mut window, Event::Key(release));
    assert!(window.is_capturing_shortcut());
}

#[test]
fn removing_and_resetting_commands() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    press_button(&mut window, &mut host, "Remove");
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Not assigned"
    );
    assert_eq!(
        window.settings().shortcuts.get("format.bold"),
        Some(&Vec::new())
    );
    assert_eq!(window.take_events(), changed_shortcuts());
    press_button(&mut window, &mut host, "Reset");
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B"
    );
    assert!(window.settings().shortcuts.is_empty());
    assert_eq!(window.take_events(), changed_shortcuts());
}

#[test]
fn reset_all_keeps_overrides_of_unknown_commands() {
    let mut settings = Settings::default();
    settings.shortcuts.insert(
        "future.command".to_owned(),
        vec![chord(key::F, true, true, false)],
    );
    settings
        .shortcuts
        .insert("format.bold".to_owned(), Vec::new());
    settings.shortcuts.insert(
        "format.italic".to_owned(),
        vec![chord(key::J, true, false, false)],
    );
    let (mut window, mut host) = open(settings);
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Not assigned"
    );
    assert_eq!(
        window.shortcut_label(CommandId::FormatItalic),
        "Italic — Ctrl+J"
    );
    select(&mut window, &mut host, "underline", "Underline — Ctrl+U");
    press_button(&mut window, &mut host, "Reset all");
    assert_eq!(window.settings().shortcuts.len(), 1);
    assert!(window.settings().shortcuts.contains_key("future.command"));
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B"
    );
    assert_eq!(window.take_events(), changed_shortcuts());
    // Replacing the settings refreshes the page.
    let mut other = Settings::default();
    other.shortcuts.insert("format.bold".to_owned(), Vec::new());
    window.set_settings(other);
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Not assigned"
    );
    assert!(!host.node(&window, Role::Button, "Reset all").state.disabled);
}

#[test]
fn the_page_is_one_tree_with_its_controls() {
    let (window, host) = open(Settings::default());
    let tree = host.tree(&window);
    assert_eq!(tree.walk().len(), tree.len());
    assert!(host.has_node(&window, Role::Edit, "Filter commands"));
    assert!(host.has_node(&window, Role::List, "Commands and their shortcuts"));
    let canvas = host.paint(&window);
    assert!(canvas.texts().contains(&"Shortcuts"));
}

#[test]
fn the_status_line_is_drawn_in_the_color_of_its_tone() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "italic", "Italic — Ctrl+I");
    let tokens = host.theme.tokens;
    let status_color = |host: &Host, window: &SettingsWindow| -> Option<Color> {
        let status = window.shortcut_status()?.to_owned();
        host.paint(window).ops().find_map(|op| match op {
            DrawOp::Text { text, color, .. } if *text == status => Some(*color),
            _ => None,
        })
    };
    // Nothing to say yet.
    assert_eq!(status_color(&host, &window), None);
    // The prompt, then the refusal, then the confirmation.
    press_button(&mut window, &mut host, "Change");
    assert_eq!(status_color(&host, &window), Some(tokens.accent));
    host.key(&mut window, key::Q, Modifiers::CTRL | Modifiers::ALT);
    assert_eq!(status_color(&host, &window), Some(tokens.error));
    host.key(&mut window, key::W, Modifiers::CTRL | Modifiers::ALT);
    assert_eq!(status_color(&host, &window), Some(tokens.text_muted));
}

#[test]
fn pointer_movement_does_not_end_a_capture() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    press_button(&mut window, &mut host, "Change");
    let moved = PointerEvent::mouse(PointerEventKind::Move, None, Point::new(5.0, 5.0));
    host.event(&mut window, Event::Pointer(moved));
    assert!(window.is_capturing_shortcut());
    assert_eq!(window.capturing_command(), Some(CommandId::FormatBold));
}

#[test]
fn a_command_sharing_several_chords_with_another_names_it_once() {
    let mut settings = Settings::default();
    let both = vec![
        chord(key::J, true, false, false),
        chord(key::J, true, true, false),
    ];
    settings
        .shortcuts
        .insert("format.bold".to_owned(), both.clone());
    settings.shortcuts.insert("format.italic".to_owned(), both);
    let (window, _) = open(settings);
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+J, Ctrl+Shift+J — conflicts with Italic"
    );
    assert_eq!(
        window.shortcut_label(CommandId::FormatItalic),
        "Italic — Ctrl+J, Ctrl+Shift+J — conflicts with Bold"
    );
    assert_eq!(window.settings().keymap().conflicts().len(), 2);
}

#[test]
fn adding_a_chord_the_command_already_has_leaves_it_bound_once() {
    let (mut window, mut host) = open(Settings::default());
    select(&mut window, &mut host, "bold", "Bold — Ctrl+B");
    press_button(&mut window, &mut host, "Add another");
    host.key(&mut window, key::B, Modifiers::CTRL);
    assert!(!window.is_capturing_shortcut());
    assert_eq!(window.shortcut_status(), Some("Bold: Ctrl+B"));
    assert_eq!(
        window.shortcut_label(CommandId::FormatBold),
        "Bold — Ctrl+B"
    );
    assert!(window.settings().shortcuts.is_empty());
}
