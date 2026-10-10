//! The Settings window: every option has a control bound two ways to `ssw_core::settings`,
//! invalid values are refused with an inline message, dependent controls follow their switch,
//! the page scrolls to the focused control, and everything is reachable through the access tree.

mod settings_support;

use settings_support::{
    Host, SETTINGS_FTL, localizer, localizer_with, settings_env, settings_window,
};
use ssw_core::settings::{Settings, SettingsSection};
use ssw_ui::access::{AccessAction, Role, ToggleState};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, Modifiers, WheelEvent, vk};
use ssw_ui::shell::settings::{
    FieldId, SettingsChange, SettingsEnv, SettingsEvent, SettingsWindow,
};
use ssw_ui::widgets::anim::Motion;

const WINDOW: Rect = Rect::new(0.0, 0.0, 900.0, 640.0);

fn open(settings: Settings, section: SettingsSection) -> (SettingsWindow, Host) {
    let mut window = settings_window(settings);
    let mut host = Host::new(WINDOW);
    window.select_section(section);
    host.layout(&mut window);
    (window, host)
}

fn label(field: FieldId) -> String {
    localizer().text(field.label_id())
}

fn changed(field: FieldId) -> Vec<SettingsEvent> {
    vec![SettingsEvent::Changed(SettingsChange::Field(field))]
}

macro_rules! toggles {
    ($(($id:ident, $sec:ident . $field:ident)),+ $(,)?) => {
        vec![$((
            FieldId::$id,
            (|s: &Settings| s.$sec.$field) as fn(&Settings) -> bool,
            (|s: &mut Settings, on: bool| s.$sec.$field = on) as fn(&mut Settings, bool),
        )),+]
    };
}

/// A toggle option: its field, how to read it from the settings and how to set it.
type ToggleEntry = (FieldId, fn(&Settings) -> bool, fn(&mut Settings, bool));

fn toggle_table() -> Vec<ToggleEntry> {
    toggles![
        (Autosave, general.autosave),
        (SyncFileName, general.sync_file_name_with_heading),
        (CheckForUpdates, general.check_for_updates),
        (Mica, appearance.mica),
        (ShowStatusBar, appearance.show_status_bar),
        (ShowFormatPill, appearance.show_format_pill),
        (AutoHideMarkdown, editor.auto_hide_markdown),
        (SpellCheck, editor.spell_check),
        (SortCompletedTasks, editor.sort_completed_tasks),
        (SyntaxHighlighting, editor.syntax_highlighting),
        (FormatTablesOnExit, editor.format_tables_on_exit),
        (HideFrontMatter, editor.hide_front_matter),
        (SidebarSortDescending, sidebar.sort_descending),
        (SidebarFoldersFirst, sidebar.folders_first),
        (SidebarShowExtensions, sidebar.show_extensions),
        (SidebarShowHidden, sidebar.show_hidden_items),
        (SidebarHideImages, sidebar.hide_images),
        (SidebarHideNonMarkdown, sidebar.hide_non_markdown),
        (SidebarShowModified, sidebar.show_modified_date),
        (LoadRemoteImages, attachments.load_remote_images),
        (ExportPageNumbers, export.page_numbers),
        (ExportBlackText, export.black_text),
        (ExportLightTheme, export.light_theme),
        (ExportIncludeTitle, export.include_title),
    ]
}

fn toggle_state(on: bool) -> Option<ToggleState> {
    Some(if on {
        ToggleState::On
    } else {
        ToggleState::Off
    })
}

#[test]
fn the_messages_are_well_formed_and_every_option_is_translated() {
    let loc = localizer();
    assert!(loc.diagnostics().is_empty(), "{:?}", loc.diagnostics());
    for field in FieldId::ALL {
        assert!(loc.has_message(field.label_id()), "{field:?}");
    }
    for section in SettingsSection::ALL {
        assert!(loc.has_message(section.label_id()), "{section:?}");
    }
}

#[test]
fn every_field_of_the_settings_model_has_a_control() {
    let json = serde_json::to_value(Settings::default()).unwrap_or_else(|e| panic!("{e}"));
    for section in SettingsSection::ALL {
        if section == SettingsSection::Shortcuts {
            continue;
        }
        let keys = json[section.key()]
            .as_object()
            .unwrap_or_else(|| panic!("{section:?} is not an object"))
            .len();
        let controls = FieldId::ALL
            .iter()
            .filter(|field| field.section() == section)
            .count();
        assert_eq!(keys, controls, "{section:?}: settings keys vs controls");
    }
}

#[test]
fn every_option_is_on_its_page_as_a_labelled_control() {
    let loc = localizer();
    for section in SettingsSection::ALL {
        if section == SettingsSection::Shortcuts {
            continue;
        }
        let (window, host) = open(Settings::default(), section);
        let tree = host.tree(&window);
        for field in FieldId::ALL.iter().filter(|f| f.section() == section) {
            let name = loc.text(field.label_id());
            let found = tree.nodes().iter().any(|node| {
                node.name == name
                    && matches!(
                        node.role,
                        Role::ToggleButton
                            | Role::CheckBox
                            | Role::ComboBox
                            | Role::Edit
                            | Role::Slider
                    )
            });
            assert!(found, "{field:?} has no control named {name:?}");
        }
    }
}

#[test]
fn toggles_write_their_setting_and_report_it() {
    for (field, get, _) in toggle_table() {
        let (mut window, mut host) = open(Settings::default(), field.section());
        let before = get(window.settings());
        let node = host.control(&window, &label(field));
        assert_eq!(node.state.toggle, toggle_state(before), "{field:?}");
        assert!(host.perform(&mut window, node.id, AccessAction::Toggle));
        assert_eq!(get(window.settings()), !before, "{field:?}");
        assert_eq!(window.take_events(), changed(field));
        assert_eq!(
            host.control(&window, &label(field)).state.toggle,
            toggle_state(!before)
        );
        assert!(host.perform(&mut window, node.id, AccessAction::Toggle));
        assert_eq!(get(window.settings()), before, "{field:?} back");
    }
}

#[test]
fn toggles_show_the_stored_value() {
    for (field, get, set) in toggle_table() {
        let mut settings = Settings::default();
        let flipped = !get(&settings);
        set(&mut settings, flipped);
        let (window, host) = open(settings, field.section());
        let node = host.control(&window, &label(field));
        assert_eq!(node.state.toggle, toggle_state(flipped), "{field:?}");
    }
}

#[test]
fn clicking_a_switch_toggles_it() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::General);
    let node = host.control(&window, &label(FieldId::CheckForUpdates));
    assert!(window.settings().general.check_for_updates);
    host.click(&mut window, node.bounds.center());
    assert!(!window.settings().general.check_for_updates);
    assert_eq!(window.take_events(), changed(FieldId::CheckForUpdates));
}

type ChoiceEntry = (FieldId, fn(&Settings) -> String, &'static str);

/// A drop-down with how to read its setting and the value after one Down key.
fn choice(field: FieldId, get: fn(&Settings) -> String, second: &'static str) -> ChoiceEntry {
    (field, get, second)
}

fn choice_table() -> Vec<ChoiceEntry> {
    vec![
        choice(
            FieldId::OnStartup,
            |s| format!("{:?}", s.general.on_startup),
            "NewDocument",
        ),
        choice(
            FieldId::NewFileLocation,
            |s| format!("{:?}", s.general.new_file_location),
            "DefaultFolder",
        ),
        choice(
            FieldId::Theme,
            |s| format!("{:?}", s.appearance.theme),
            "Light",
        ),
        choice(
            FieldId::Accent,
            |s| format!("{:?}", s.appearance.accent),
            "Blue",
        ),
        choice(
            FieldId::ListIndent,
            |s| format!("{:?}", s.editor.list_indent),
            "Two",
        ),
        choice(
            FieldId::SidebarSortBy,
            |s| format!("{:?}", s.sidebar.sort_by),
            "DateModified",
        ),
        choice(
            FieldId::AttachmentLocation,
            |s| format!("{:?}", s.attachments.location),
            "AssetsFolder",
        ),
        choice(
            FieldId::ExportPageSize,
            |s| format!("{:?}", s.export.page_size),
            "Letter",
        ),
        choice(
            FieldId::ExportDefaultFormat,
            |s| format!("{:?}", s.export.default_format),
            "Png",
        ),
    ]
}

#[test]
fn drop_downs_write_their_setting_and_show_the_stored_one() {
    for (field, get, second) in choice_table() {
        let (mut window, mut host) = open(Settings::default(), field.section());
        let name = label(field);
        let first_text = host.control(&window, &name).value.unwrap_or_default();
        assert_ne!(get(window.settings()), second, "{field:?} starts elsewhere");
        host.focus_control(&mut window, &name);
        host.key(&mut window, vk::DOWN, Modifiers::NONE);
        assert_eq!(get(window.settings()), second, "{field:?}");
        assert_eq!(window.take_events(), changed(field));
        let second_text = host.control(&window, &name).value.unwrap_or_default();
        assert_ne!(first_text, second_text, "{field:?} shows the new choice");
        // A window built from the stored value shows the same choice.
        let (reopened, host) = open(window.settings().clone(), field.section());
        assert_eq!(
            host.control(&reopened, &name).value.as_deref(),
            Some(second_text.as_str())
        );
    }
}

#[test]
fn the_language_drop_down_stores_the_tag_and_keeps_unknown_ones() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::General);
    let name = label(FieldId::Language);
    assert_eq!(
        host.control(&window, &name).value.as_deref(),
        Some("System default")
    );
    host.focus_control(&mut window, &name);
    host.key(&mut window, vk::DOWN, Modifiers::NONE);
    assert_eq!(window.settings().general.language.as_deref(), Some("en"));
    host.key(&mut window, vk::DOWN, Modifiers::NONE);
    assert_eq!(window.settings().general.language.as_deref(), Some("tr"));
    assert_eq!(
        host.control(&window, &name).value.as_deref(),
        Some("Türkçe")
    );
    host.key(&mut window, vk::HOME, Modifiers::NONE);
    assert_eq!(window.settings().general.language, None);

    let mut settings = Settings::default();
    settings.general.language = Some("de".to_owned());
    let (window, host) = open(settings, SettingsSection::General);
    assert_eq!(host.control(&window, &name).value.as_deref(), Some("de"));
}

type Check = fn(&Settings) -> bool;
type TypedEntry = (FieldId, &'static str, Check);

/// A text control, a valid text for it and how to check the stored setting.
fn typed(field: FieldId, text: &'static str, check: Check) -> TypedEntry {
    (field, text, check)
}

fn typed_values() -> Vec<TypedEntry> {
    vec![
        typed(FieldId::TextFont, "Georgia", |s| {
            s.editor.text_font == "Georgia"
        }),
        typed(FieldId::HeadingFont, " Segoe UI Semibold ", |s| {
            s.editor.heading_font == "Segoe UI Semibold"
        }),
        typed(FieldId::CodeFont, "Consolas", |s| {
            s.editor.code_font == "Consolas"
        }),
        typed(FieldId::DefaultFolder, "C:\\Notes", |s| {
            s.general.default_folder.as_deref() == Some("C:\\Notes")
        }),
        typed(FieldId::NamingTemplate, "shot-{n}", |s| {
            s.attachments.naming_template == "shot-{n}"
        }),
        typed(FieldId::SpellLanguages, "en-US, tr", |s| {
            s.editor.spell_languages == ["en-US", "tr"]
        }),
        typed(FieldId::AutosaveDelay, "2500", |s| {
            s.general.autosave_delay_ms == 2500
        }),
        typed(FieldId::SidebarFontSize, "14.5", |s| {
            s.appearance.sidebar_font_size == 14.5
        }),
        typed(FieldId::ZoomPercent, "150", |s| {
            s.appearance.zoom_percent == 150
        }),
        typed(FieldId::FontSize, "18", |s| s.editor.font_size == 18.0),
        typed(FieldId::LineHeight, "1,5", |s| s.editor.line_height == 1.5),
        typed(FieldId::ParagraphSpacing, "0.75", |s| {
            s.editor.paragraph_spacing == 0.75
        }),
        typed(FieldId::LineWidth, "800", |s| s.editor.line_width == 800),
        typed(FieldId::ReadingSpeed, "250", |s| {
            s.editor.reading_speed_wpm == 250
        }),
        typed(FieldId::ExportMargin, "25", |s| s.export.margin_mm == 25.0),
        typed(FieldId::ExportFontSize, "12", |s| {
            s.export.base_font_size_pt == 12.0
        }),
        typed(FieldId::IndexMaxFiles, "5000", |s| {
            s.advanced.index_max_files == 5000
        }),
        typed(FieldId::HistoryRetention, "90", |s| {
            s.advanced.history_retention_days == 90
        }),
    ]
}

#[test]
fn typed_values_are_stored_and_shown_back() {
    for (field, text, check) in typed_values() {
        let (mut window, mut host) = open(Settings::default(), field.section());
        let node = host.control(&window, &label(field));
        assert!(!check(window.settings()), "{field:?} starts elsewhere");
        assert!(host.perform(
            &mut window,
            node.id,
            AccessAction::SetValue(text.to_owned())
        ));
        assert!(check(window.settings()), "{field:?} = {text:?}");
        assert_eq!(window.error(field), None);
        assert_eq!(window.take_events(), changed(field));
        // A window built from the stored settings shows the value.
        let (reopened, host) = open(window.settings().clone(), field.section());
        let shown = host
            .control(&reopened, &label(field))
            .value
            .unwrap_or_default();
        let reparsed = {
            let (mut again, mut again_host) = open(Settings::default(), field.section());
            let node = again_host.control(&again, &label(field));
            again_host.perform(&mut again, node.id, AccessAction::SetValue(shown.clone()));
            check(again.settings())
        };
        assert!(reparsed, "{field:?} shown as {shown:?}");
    }
}

#[test]
fn refused_values_keep_the_setting_and_show_a_message_until_fixed() {
    let refused: Vec<(FieldId, &str, &str)> = vec![
        (FieldId::TextFont, "  ", "This cannot be empty."),
        (
            FieldId::DefaultFolder,
            "C:\\No?pe",
            "Windows does not allow < > \" | ? * in a path.",
        ),
        (
            FieldId::NamingTemplate,
            "shot",
            "The name must contain {n} and none of \\ / : * ? \" < > |",
        ),
        (
            FieldId::SpellLanguages,
            "english!",
            "english! is not a language tag. Examples: en-US, tr.",
        ),
        (
            FieldId::AutosaveDelay,
            "50",
            "Enter a value from 100 to 60000.",
        ),
        (FieldId::AutosaveDelay, "abc", "Enter a number."),
        (FieldId::AutosaveDelay, "1.5", "Enter a whole number."),
        (FieldId::AutosaveDelay, "NaN", "Enter a number."),
        (
            FieldId::SidebarFontSize,
            "30",
            "Enter a value from 9 to 24.",
        ),
        (FieldId::LineHeight, "0.5", "Enter a value from 1 to 3."),
        (FieldId::ExportMargin, "-1", "Enter a value from 0 to 100."),
    ];
    for (field, text, message) in refused {
        let (mut window, mut host) = open(Settings::default(), field.section());
        let node = host.control(&window, &label(field));
        assert!(host.perform(
            &mut window,
            node.id,
            AccessAction::SetValue(text.to_owned())
        ));
        assert_eq!(
            window.settings(),
            &Settings::default(),
            "{field:?} {text:?}"
        );
        assert_eq!(window.error(field), Some(message), "{field:?} {text:?}");
        assert!(window.take_events().is_empty());
        assert!(
            host.has_node(&window, Role::Text, message),
            "{message:?} is exposed"
        );
        // The message is drawn in the error color.
        let canvas = host.paint(&window);
        let error_color = host.theme.tokens.error;
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::Text { text, color, .. } if text == message && *color == error_color
        )));
        // Entering a valid value clears it.
        let valid = typed_values().into_iter().find(|(f, _, _)| *f == field);
        if let Some((_, good, _)) = valid {
            host.perform(
                &mut window,
                node.id,
                AccessAction::SetValue(good.to_owned()),
            );
            assert_eq!(window.error(field), None);
            assert!(!host.has_node(&window, Role::Text, message));
            assert_eq!(window.take_events(), changed(field));
        }
    }
}

#[test]
fn sliders_store_percentages() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Editor);
    let position = host.control(&window, &label(FieldId::TypewriterPosition));
    assert_eq!(position.role, Role::Slider);
    assert_eq!(position.patterns.range_value.map(|r| r.value), Some(50.0));
    assert!(host.perform(&mut window, position.id, AccessAction::SetRangeValue(30.0)));
    assert_eq!(window.settings().editor.typewriter_position, 0.3);
    assert_eq!(window.take_events(), changed(FieldId::TypewriterPosition));
    let zone = host.control(&window, &label(FieldId::TypewriterDeadZone));
    assert_eq!(zone.patterns.range_value.map(|r| r.max), Some(50.0));
    assert!(host.perform(&mut window, zone.id, AccessAction::SetRangeValue(20.0)));
    assert_eq!(window.settings().editor.typewriter_dead_zone, 0.2);

    let mut stored = Settings::default();
    stored.editor.typewriter_position = 0.25;
    let (window, host) = open(stored, SettingsSection::Editor);
    let shown = host.control(&window, &label(FieldId::TypewriterPosition));
    assert_eq!(shown.patterns.range_value.map(|r| r.value), Some(25.0));
    // The value is drawn beside the slider.
    assert!(host.paint(&window).texts().contains(&"25%"));
}

#[test]
fn dependent_controls_follow_their_switch() {
    // The autosave delay needs autosave.
    let (mut window, mut host) = open(Settings::default(), SettingsSection::General);
    let delay = label(FieldId::AutosaveDelay);
    assert!(!host.control(&window, &delay).state.disabled);
    let switch = host.control(&window, &label(FieldId::Autosave));
    host.perform(&mut window, switch.id, AccessAction::Toggle);
    let disabled = host.control(&window, &delay);
    assert!(disabled.state.disabled);
    assert!(!host.perform(
        &mut window,
        disabled.id,
        AccessAction::SetValue("900".into())
    ));
    host.perform(&mut window, switch.id, AccessAction::Toggle);
    assert!(!host.control(&window, &delay).state.disabled);

    // The custom accent needs the custom mode.
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Appearance);
    let custom = label(FieldId::AccentCustom);
    assert!(host.control(&window, &custom).state.disabled);
    host.focus_control(&mut window, &label(FieldId::Accent));
    host.key(&mut window, vk::END, Modifiers::NONE);
    let node = host.control(&window, &custom);
    assert!(!node.state.disabled);
    host.perform(
        &mut window,
        node.id,
        AccessAction::SetValue("#1a2b3c".into()),
    );
    assert_eq!(
        window.settings().appearance.accent_custom.as_deref(),
        Some("#1A2B3C")
    );
    host.perform(&mut window, node.id, AccessAction::SetValue("blue".into()));
    assert_eq!(
        window.error(FieldId::AccentCustom),
        Some("Write the color as #RRGGBB.")
    );
    assert_eq!(
        window.settings().appearance.accent_custom.as_deref(),
        Some("#1A2B3C")
    );
    host.perform(&mut window, node.id, AccessAction::SetValue(String::new()));
    assert_eq!(window.settings().appearance.accent_custom, None);

    // The custom attachment folder needs the custom location and may not be empty then.
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Attachments);
    let folder = label(FieldId::CustomFolder);
    assert!(host.control(&window, &folder).state.disabled);
    host.focus_control(&mut window, &label(FieldId::AttachmentLocation));
    host.key(&mut window, vk::END, Modifiers::NONE);
    let node = host.control(&window, &folder);
    assert!(!node.state.disabled);
    host.perform(&mut window, node.id, AccessAction::SetValue(String::new()));
    assert_eq!(
        window.error(FieldId::CustomFolder),
        Some("This cannot be empty.")
    );
    host.perform(
        &mut window,
        node.id,
        AccessAction::SetValue("D:\\Pics".into()),
    );
    assert_eq!(window.settings().attachments.custom_folder, "D:\\Pics");
    assert_eq!(window.error(FieldId::CustomFolder), None);

    // Spell-check languages need spell check.
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Editor);
    let switch = host.control(&window, &label(FieldId::SpellCheck));
    host.perform(&mut window, switch.id, AccessAction::Toggle);
    assert!(
        host.control(&window, &label(FieldId::SpellLanguages))
            .state
            .disabled
    );
}

#[test]
fn folder_rows_ask_the_app_to_browse() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::General);
    let browse = host.node(&window, Role::Button, "Browse…");
    assert!(host.perform(&mut window, browse.id, AccessAction::Invoke));
    assert_eq!(
        window.take_events(),
        vec![SettingsEvent::BrowseFolder(FieldId::DefaultFolder)]
    );
    window.set_folder(FieldId::DefaultFolder, "C:\\Docs");
    host.layout(&mut window);
    assert_eq!(
        window.settings().general.default_folder.as_deref(),
        Some("C:\\Docs")
    );
    assert_eq!(window.take_events(), changed(FieldId::DefaultFolder));
    assert_eq!(
        host.control(&window, &label(FieldId::DefaultFolder))
            .value
            .as_deref(),
        Some("C:\\Docs")
    );
    window.set_folder(FieldId::DefaultFolder, "C:\\Do|cs");
    assert_eq!(
        window.settings().general.default_folder.as_deref(),
        Some("C:\\Docs")
    );
    assert!(window.error(FieldId::DefaultFolder).is_some());
}

#[test]
fn advanced_actions_are_events_and_reset_replaces_the_settings() {
    let mut settings = Settings::default();
    settings.general.autosave = false;
    settings.shortcuts.insert("file.new".to_owned(), Vec::new());
    let (mut window, mut host) = open(settings, SettingsSection::Advanced);
    let logs = host.node(&window, Role::Button, "Open logs folder");
    host.perform(&mut window, logs.id, AccessAction::Invoke);
    assert_eq!(window.take_events(), vec![SettingsEvent::OpenLogsFolder]);
    let file = host.node(&window, Role::Button, "Open settings.json");
    host.perform(&mut window, file.id, AccessAction::Invoke);
    assert_eq!(window.take_events(), vec![SettingsEvent::OpenSettingsFile]);
    let reset = host.node(&window, Role::Button, "Reset all settings");
    assert_eq!(
        host.control(&window, "Reset all settings").description,
        "Puts every setting back to its default, shortcuts included."
    );
    host.perform(&mut window, reset.id, AccessAction::Invoke);
    assert_eq!(window.take_events(), vec![SettingsEvent::ResetSettings]);
    // The app confirms and replaces the settings: every page shows the defaults again.
    window.set_settings(Settings::default());
    host.layout(&mut window);
    assert_eq!(window.settings(), &Settings::default());
    window.select_section(SettingsSection::General);
    host.layout(&mut window);
    assert_eq!(
        host.control(&window, &label(FieldId::Autosave))
            .state
            .toggle,
        toggle_state(true)
    );
    assert_eq!(
        window.shortcut_label(ssw_core::command::CommandId::FileNew),
        "New file — Ctrl+N"
    );
    assert_eq!(
        host.tree(&window).root().map(|n| n.name.as_str()),
        Some("Settings")
    );
}

#[test]
fn the_navigation_list_switches_pages() {
    let mut window = settings_window(Settings::default());
    let mut host = Host::new(WINDOW);
    host.layout(&mut window);
    assert_eq!(window.section(), SettingsSection::General);
    let nav = host.tree(&window);
    let rows: Vec<&str> = nav
        .nodes()
        .iter()
        .filter(|n| n.role == Role::ListItem)
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(
        rows,
        [
            "General",
            "Appearance",
            "Editor",
            "Sidebar",
            "Attachments",
            "Export",
            "Shortcuts",
            "Advanced"
        ]
    );
    let editor = host.node(&window, Role::ListItem, "Editor");
    host.click(&mut window, editor.bounds.center());
    assert_eq!(window.section(), SettingsSection::Editor);
    assert!(host.paint(&window).texts().contains(&"Editor"));
    host.control(&window, &label(FieldId::FontSize));
    // The keyboard moves through the list too.
    let list = host.node(&window, Role::List, "Settings pages");
    assert!(host.perform(&mut window, list.id, AccessAction::Focus));
    host.key(&mut window, vk::DOWN, Modifiers::NONE);
    assert_eq!(window.section(), SettingsSection::Sidebar);
    host.control(&window, &label(FieldId::SidebarSortBy));
    // Selecting a row through accessibility works as well.
    let advanced = host.node(&window, Role::ListItem, "Advanced");
    assert!(host.perform(&mut window, advanced.id, AccessAction::Select));
    assert_eq!(window.section(), SettingsSection::Advanced);
    window.select_section(SettingsSection::Export);
    assert_eq!(window.section(), SettingsSection::Export);
}

#[test]
fn the_page_scrolls_to_the_focused_control_and_by_wheel() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Editor);
    let last = label(FieldId::ReadingSpeed);
    assert!(host.control(&window, &last).state.offscreen);
    let focused = host.focus_control(&mut window, &last);
    assert!(!focused.state.offscreen);
    assert!(focused.bounds.bottom() <= WINDOW.bottom());
    // The user's own scrolling is not undone by the next event.
    let inside = Point::new(500.0, 300.0);
    let before = host.control(&window, &last).bounds.y;
    host.wheel(&mut window, inside, 40.0);
    let after = host.control(&window, &last).bounds.y;
    assert!(
        (after - (before + 40.0)).abs() < 0.01,
        "{before} -> {after}"
    );
    host.key(&mut window, vk::SHIFT, Modifiers::NONE);
    assert_eq!(host.control(&window, &last).bounds.y, after);
    // The scroll bar is exposed while the page is longer than the window.
    assert!(host.has_node(&window, Role::ScrollBar, "Page scroll bar"));
    // Wheeling outside the page does nothing.
    let result = host.wheel(&mut window, Point::new(5.0, 5.0), -40.0);
    assert_eq!(host.control(&window, &last).bounds.y, after);
    assert!(!result.handled);
    // A short page has no scroll bar worth exposing.
    let (window, host) = open(Settings::default(), SettingsSection::Attachments);
    let bar = host
        .tree(&window)
        .nodes()
        .iter()
        .any(|n| n.role == Role::ScrollBar);
    assert!(!bar);
}

#[test]
fn open_drop_downs_stay_in_view() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Editor);
    let indent = label(FieldId::ListIndent);
    let node = host.control(&window, &indent);
    assert!(host.perform(&mut window, node.id, AccessAction::Expand));
    let opened = host.control(&window, &indent);
    assert_eq!(opened.state.expanded, Some(true));
    let tree = host.tree(&window);
    let option = tree
        .nodes()
        .iter()
        .find(|n| n.role == Role::ListItem && n.name == "4 spaces")
        .unwrap_or_else(|| panic!("popup rows are exposed"));
    assert!(!option.state.offscreen);
    // Clicking an option commits it.
    let at = option.bounds.center();
    host.click(&mut window, at);
    assert_eq!(
        format!("{:?}", window.settings().editor.list_indent),
        "Four"
    );
}

#[test]
fn the_window_can_be_shown_in_another_language() {
    let translated = SETTINGS_FTL
        .replace(
            "settings-general-autosave = Autosave",
            "settings-general-autosave = Otomatik kaydet",
        )
        .replace("settings-title = Settings", "settings-title = Ayarlar");
    let (mut window, mut host) = open(Settings::default(), SettingsSection::General);
    assert!(host.has_node(&window, Role::Window, "Settings"));
    window.relocalize(localizer_with(&translated));
    host.layout(&mut window);
    assert!(host.has_node(&window, Role::Window, "Ayarlar"));
    let node = host.control(&window, "Otomatik kaydet");
    assert_eq!(node.state.toggle, toggle_state(true));
    assert_eq!(window.section(), SettingsSection::General);
}

#[test]
fn the_access_tree_is_one_connected_tree_on_every_page() {
    for section in SettingsSection::ALL {
        let (window, host) = open(Settings::default(), section);
        let tree = host.tree(&window);
        let root = tree.root().unwrap_or_else(|| panic!("no root"));
        assert_eq!(root.role, Role::Window);
        assert_eq!(root.name, "Settings");
        assert_eq!(root.children.len(), 2);
        assert_eq!(
            tree.walk().len(),
            tree.len(),
            "{section:?}: unreachable nodes"
        );
        let ids: std::collections::HashSet<_> = tree.nodes().iter().map(|n| n.id).collect();
        assert_eq!(ids.len(), tree.len(), "{section:?}: duplicate ids");
    }
}

#[test]
fn the_window_paints_balanced_with_its_page_title() {
    let (window, host) = open(Settings::default(), SettingsSection::Sidebar);
    let canvas = host.paint(&window);
    assert!(canvas.texts().contains(&"Sidebar"));
    assert!(canvas.texts().contains(&"Folders first"));
    let tokens = &host.theme.tokens;
    assert!(canvas.ops().any(|op| matches!(
        op,
        DrawOp::FillRect { color, rect } if *color == tokens.sidebar_bg && rect.width == 220.0
    )));
}

#[test]
fn the_debug_forms_name_the_section_and_the_shipped_languages() {
    let (window, _host) = open(Settings::default(), SettingsSection::Sidebar);
    let debug = format!("{window:?}");
    assert!(debug.starts_with("SettingsWindow {"), "{debug}");
    assert!(debug.contains("section: Sidebar"), "{debug}");
    let env = format!("{:?}", settings_env());
    assert!(env.starts_with("SettingsEnv {"), "{env}");
    assert!(env.contains("English") && env.contains("Türkçe"), "{env}");
}

#[test]
fn the_window_measures_as_the_area_it_is_given_or_a_default_size() {
    let (mut window, mut host) = open(Settings::default(), SettingsSection::Sidebar);
    assert_eq!(
        host.measure(&mut window, Size::new(1000.0, 700.0)),
        Size::new(1000.0, 700.0)
    );
    let default = host.measure(&mut window, Size::UNBOUNDED);
    assert!(default.width.is_finite() && default.height.is_finite());
    assert_eq!(
        host.measure(&mut window, Size::new(f32::INFINITY, 700.0)),
        Size::new(default.width, 700.0)
    );
    assert_eq!(
        host.measure(&mut window, Size::new(1000.0, f32::INFINITY)),
        Size::new(1000.0, default.height)
    );
}

#[test]
fn nothing_is_painted_outside_the_damaged_area() {
    let (window, host) = open(Settings::default(), SettingsSection::Sidebar);
    let away = Rect::new(2000.0, 2000.0, 10.0, 10.0);
    assert_eq!(host.paint_damaged(&window, Some(away)).ops().count(), 0);
    assert!(host.paint_damaged(&window, Some(WINDOW)).ops().count() > 0);
}

#[test]
fn enter_in_a_text_box_changes_nothing_by_itself() {
    let field = FieldId::TextFont;
    let (mut window, mut host) = open(Settings::default(), field.section());
    host.focus_control(&mut window, &label(field));
    let before = window.settings().clone();
    host.key(&mut window, vk::RETURN, Modifiers::NONE);
    assert_eq!(window.settings(), &before);
    assert!(window.take_events().is_empty());
}

#[test]
fn a_partial_repaint_skips_the_rows_outside_the_damage() {
    let (window, host) = open(Settings::default(), SettingsSection::Editor);
    let everything = host.paint(&window).ops().count();
    let top = Rect::new(0.0, 0.0, WINDOW.width, 100.0);
    let partial = host.paint_damaged(&window, Some(top)).ops().count();
    assert!(
        partial > 0 && partial < everything,
        "{partial} of {everything}"
    );
}

#[test]
fn animation_frames_ease_the_page_after_a_notched_wheel() {
    let env = SettingsEnv {
        motion: Motion::full(),
        ..settings_env()
    };
    let mut window = SettingsWindow::new(Settings::default(), localizer(), env);
    let mut host = Host::new(WINDOW);
    window.select_section(SettingsSection::Editor);
    host.layout(&mut window);
    let last = label(FieldId::ReadingSpeed);
    let before = host.control(&window, &last).bounds.y;
    let wheel = Event::Wheel(WheelEvent {
        delta: Point::new(0.0, -3.0),
        precise: false,
        position: Point::new(500.0, 300.0),
        mods: Modifiers::NONE,
    });
    assert!(host.event(&mut window, wheel).handled);
    // Nothing has moved until the first frame fixes the start of the easing.
    assert_eq!(host.control(&window, &last).bounds.y, before);
    for millis in [100_u64, 200, 5_000] {
        let frame = Event::AnimationFrame(std::time::Duration::from_millis(millis));
        host.event(&mut window, frame);
    }
    let after = host.control(&window, &last).bounds.y;
    assert!(after < before, "{before} -> {after}");
}
