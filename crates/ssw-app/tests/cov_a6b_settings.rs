//! Corners of the Settings window and of the life of the settings behind it: how the theme is
//! resolved without a system theme, edits that changed nothing or cannot be read back, the
//! focus keys, relocalizing, the settings an open window is brought up to date with, the file
//! that disappears, the write that fails and is retried. Runs the real controller over a
//! temporary profile with scripted dialogs and shell (no native window).

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ssw_app::app::AppCore;
use ssw_app::args::Args;
use ssw_app::i18n::localizer_for;
use ssw_app::startup::{Boot, ProcessEnv, boot};
use ssw_app::ui::controller::Controller;
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::ui::settings_window::{
    SettingsCell, SettingsSetup, WINDOW_SIZE, merge_changes, resolve_theme,
};
use ssw_app::workers::DeferredWaker;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::settings::{Appearance, Settings, SettingsSection, Theme as ThemeSetting};
use ssw_io::paths::Environment;
use ssw_io::settings_file::{load_settings, save_settings};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::geometry::Rect;
use ssw_ui::input::{Event, KeyEvent, Modifiers, vk};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::settings::{DisplayKeyNames, FieldId, SettingsChange, SettingsEvent};
use ssw_ui::shortcuts::CharProbe;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widgets::SharedClipboard;
use ssw_ui::widgets::anim::Motion;
use ssw_win::chrome::CAPTION_HEIGHT;
use ssw_win::gfx::Driver;
use ssw_win::theme::SystemTheme;
use ssw_win::window::OsVersion;

struct NoAltGr;

impl CharProbe for NoAltGr {
    fn produces_char(&self, _vk: u16, _scan: u16, _shift: bool, _caps: bool) -> bool {
        false
    }
}

struct Fixture {
    controller: Rc<Controller>,
    services: ScriptedServices,
    clock: Rc<FakeClock>,
    file: PathBuf,
    _tmp: tempfile::TempDir,
}

/// A headless controller over a temporary profile whose `settings.json` holds `json`.
fn fixture(json: Option<&str>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let dir = root.join("roaming").join("SS Writer");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("settings.json");
    if let Some(json) = json {
        std::fs::write(&file, json).unwrap();
    }
    let process = ProcessEnv {
        os: OsVersion {
            major: 10,
            build: 22621,
        },
        env: Environment {
            local_app_data: Some(root.join("local")),
            app_data: Some(root.join("roaming")),
            exe_dir: root.join("exe"),
        },
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    };
    let services = ScriptedServices::new();
    let mut dialogs = services.dialogs();
    let core = match boot(&Args::default(), process, &mut *dialogs, None).unwrap() {
        Boot::Ready(started) => AppCore::new(*started, Arc::new(DeferredWaker::new())),
        Boot::Exit(code) => panic!("startup exited with {code}"),
    };
    let plan = core.plan.clone();
    let clock = Rc::new(FakeClock::default());
    let time: Rc<dyn Clock> = clock.clone();
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    let controller = Controller::new(core, services.dialogs(), services.shell(), clipboard, time);
    controller.open_plan(&plan);
    Fixture {
        controller,
        services,
        clock,
        file,
        _tmp: tmp,
    }
}

fn setup() -> SettingsSetup {
    SettingsSetup {
        probe: Box::new(NoAltGr),
        key_names: Box::new(DisplayKeyNames),
        motion: Motion::reduced(),
        size: WINDOW_SIZE,
        scale: 1.0,
    }
}

/// Changes the control labelled `field` of the open Settings window like a user would (through
/// its accessibility action) and hands the resulting events to the controller.
fn change(fx: &Fixture, section: SettingsSection, field: FieldId, action: AccessAction) {
    let cell = fx.controller.settings_cell().expect("settings are open");
    let engine = MonoTextEngine::default();
    let label = localizer_for("en").text(field.label_id());
    let events = {
        let mut view = cell.view.borrow_mut();
        view.window_mut().select_section(section);
        // One pass so the page of the section exists in the tree.
        let _ = view.event(&Event::AnimationFrame(Duration::ZERO), &engine);
        let tree = view.access_tree(&engine);
        let node = tree
            .nodes()
            .iter()
            .find(|node| {
                node.name == label
                    && !matches!(
                        node.role,
                        Role::Group
                            | Role::Window
                            | Role::Pane
                            | Role::List
                            | Role::ListItem
                            | Role::Text
                    )
            })
            .cloned()
            .unwrap_or_else(|| panic!("no control named {label:?}"));
        view.perform_access(node.id, &action, &engine)
    };
    assert!(!events.is_empty(), "{field:?} reported nothing");
    for event in events {
        fx.controller.settings_event(event);
    }
}

fn font_size(fx: &Fixture) -> f32 {
    fx.controller.settings().editor.font_size
}

fn written(fx: &Fixture) -> Settings {
    load_settings(&fx.file).unwrap().settings
}

fn light_system() -> SystemTheme {
    SystemTheme {
        theme: Theme::light(DEFAULT_ACCENT),
        dark: false,
        high_contrast: false,
        transparency_effects: true,
        client_animations: true,
        accent: DEFAULT_ACCENT,
    }
}

// ----- the theme ------------------------------------------------------------------------------

#[test]
fn the_light_setting_wins_over_a_dark_system() {
    let dark_system = SystemTheme {
        theme: Theme::dark(DEFAULT_ACCENT),
        dark: true,
        ..light_system()
    };
    let appearance = Appearance {
        theme: ThemeSetting::Light,
        ..Appearance::default()
    };
    assert_eq!(
        resolve_theme(&appearance, &dark_system),
        Theme::light(DEFAULT_ACCENT)
    );
}

#[test]
fn settings_resolve_against_a_light_built_in_system_until_the_system_reports_one() {
    let fx = fixture(None);
    let mut settings = fx.controller.settings().clone();
    settings.appearance.theme = ThemeSetting::Dark;
    fx.controller.install_settings(settings.clone(), false);
    assert_eq!(fx.controller.theme(), Theme::dark(DEFAULT_ACCENT));
    settings.appearance.theme = ThemeSetting::System;
    fx.controller.install_settings(settings, false);
    assert_eq!(fx.controller.theme(), Theme::light(DEFAULT_ACCENT));
}

// ----- merging --------------------------------------------------------------------------------

#[test]
fn an_edit_that_changed_nothing_keeps_the_current_settings() {
    let mut current = Settings::default();
    current.appearance.zoom_percent = 130;
    current.editor.font_size = 19.0;
    let base = Settings::default();
    assert_eq!(merge_changes(&current, &base, &base), current);
}

#[test]
fn settings_that_cannot_be_read_back_after_the_merge_fall_back_to_the_edited_ones() {
    // A stray top-level key named like a section shadows the section when the settings are
    // written out: what the merge produces is not settings any more.
    let mut current = Settings::default();
    current
        .extra
        .insert("general".to_owned(), serde_json::json!(5));
    let base = Settings::default();
    let mut edited = base.clone();
    edited.editor.font_size = 21.0;
    assert_eq!(merge_changes(&current, &base, &edited), edited);
}

#[test]
fn a_stale_window_is_brought_up_to_date_with_what_the_app_changed_meanwhile() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let mut app_side = fx.controller.settings().clone();
    app_side.appearance.zoom_percent = 120;
    fx.controller.install_settings(app_side, false);
    let zoom_shown = || {
        cell.view
            .borrow()
            .window()
            .settings()
            .appearance
            .zoom_percent
    };
    assert_ne!(zoom_shown(), 120, "the window has not been told");

    change(
        &fx,
        SettingsSection::Editor,
        FieldId::FontSize,
        AccessAction::SetValue("20".to_owned()),
    );
    // The edit is merged into the settings in force (zoom kept) and the window shows them.
    assert_eq!(fx.controller.settings().appearance.zoom_percent, 120);
    assert_eq!(font_size(&fx), 20.0);
    assert_eq!(zoom_shown(), 120);
    assert_eq!(
        cell.view.borrow().window().settings().editor.font_size,
        20.0
    );
}

// ----- the view -------------------------------------------------------------------------------

#[test]
fn the_caption_strip_is_only_filled_when_there_is_no_backdrop() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let engine = MonoTextEngine::default();
    let strip = Rect::new(0.0, 0.0, WINDOW_SIZE.width, CAPTION_HEIGHT);
    let fills_strip = |mica: bool| {
        let mut canvas = RecordingCanvas::new();
        cell.view.borrow_mut().paint(&mut canvas, &engine, mica);
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::FillRect { rect, .. } if *rect == strip))
    };
    assert!(fills_strip(false));
    assert!(!fills_strip(true));
}

#[test]
fn a_translated_title_reaches_the_view_and_its_accessibility_tree() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let engine = MonoTextEngine::default();
    assert_eq!(cell.view.borrow().title(), "Settings");
    let turkish = Localizer::new("tr", &[("tr", "settings-title = Ayarlar\n")]);
    {
        let mut view = cell.view.borrow_mut();
        let _ = view.requests_mut().take_invalidation();
        view.relocalize(Rc::new(turkish));
        assert!(
            !view.requests_mut().take_invalidation().is_clean(),
            "the window repaints in the new language"
        );
    }
    let mut view = cell.view.borrow_mut();
    assert_eq!(view.title(), "Ayarlar");
    let _ = view.event(&Event::AnimationFrame(Duration::ZERO), &engine);
    let tree = view.access_tree(&engine);
    assert!(
        tree.nodes()
            .iter()
            .any(|node| node.role == Role::Window && node.name == "Ayarlar")
    );
}

/// The name of the control that has the keyboard focus in the Settings window.
fn focused_name(cell: &SettingsCell, engine: &MonoTextEngine) -> String {
    let view = cell.view.borrow();
    let tree = view.access_tree(engine);
    tree.nodes()
        .iter()
        .find(|node| node.state.focused)
        .map(|node| node.name.clone())
        .expect("a control has the focus")
}

#[test]
fn tab_and_shift_tab_move_the_focus_when_no_control_wants_the_key() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let engine = MonoTextEngine::default();
    let press = |mods: Modifiers| {
        cell.view
            .borrow_mut()
            .event(&Event::Key(KeyEvent::press(vk::TAB, mods)), &engine)
    };
    // The first pass gives the first control the focus.
    let _ = cell
        .view
        .borrow_mut()
        .event(&Event::AnimationFrame(Duration::ZERO), &engine);
    let first = focused_name(&cell, &engine);
    let _ = cell.view.borrow_mut().requests_mut().take_invalidation();
    assert!(press(Modifiers::NONE).is_empty());
    let second = focused_name(&cell, &engine);
    assert_ne!(first, second);
    assert!(
        !cell
            .view
            .borrow_mut()
            .requests_mut()
            .take_invalidation()
            .is_clean(),
        "the focus change is repainted"
    );
    assert!(press(Modifiers::SHIFT).is_empty());
    assert_eq!(focused_name(&cell, &engine), first);
}

#[test]
fn picking_a_language_stores_the_tag_and_relocalizes_the_window() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let engine = MonoTextEngine::default();
    let label = localizer_for("en").text(FieldId::Language.label_id());
    let events = {
        let mut view = cell.view.borrow_mut();
        view.window_mut().select_section(SettingsSection::General);
        let _ = view.event(&Event::AnimationFrame(Duration::ZERO), &engine);
        let node = view
            .access_tree(&engine)
            .nodes()
            .iter()
            .find(|node| node.name == label && node.role == Role::ComboBox)
            .cloned()
            .expect("the language drop-down");
        let _ = view.perform_access(node.id, &AccessAction::Focus, &engine);
        view.event(
            &Event::Key(KeyEvent::press(vk::DOWN, Modifiers::NONE)),
            &engine,
        )
    };
    assert_eq!(
        events,
        [SettingsEvent::Changed(SettingsChange::Field(
            FieldId::Language
        ))]
    );
    let _ = cell.view.borrow_mut().requests_mut().take_invalidation();
    for event in events {
        fx.controller.settings_event(event);
    }
    assert_eq!(
        fx.controller.settings().general.language.as_deref(),
        Some("en")
    );
    assert!(
        !cell
            .view
            .borrow_mut()
            .requests_mut()
            .take_invalidation()
            .is_clean(),
        "the window is shown again in the chosen language"
    );
    assert_eq!(cell.view.borrow().title(), "Settings");
}

#[test]
fn the_status_bar_and_the_format_pill_follow_their_switches() {
    let fx = fixture(None);
    let _cell = fx.controller.create_settings(setup());
    let window = fx.controller.windows()[0].clone();
    let status = || window.view.borrow().layout().status_bar_visible();
    let pill = || window.view.borrow().titlebar().pill_visible();
    let (status_before, pill_before) = (status(), pill());

    change(
        &fx,
        SettingsSection::Appearance,
        FieldId::ShowStatusBar,
        AccessAction::Toggle,
    );
    assert_eq!(status(), !status_before);
    assert_eq!(
        fx.controller.settings().appearance.show_status_bar,
        !status_before
    );
    assert_eq!(pill(), pill_before, "the pill is left alone");

    change(
        &fx,
        SettingsSection::Appearance,
        FieldId::ShowFormatPill,
        AccessAction::Toggle,
    );
    assert_eq!(pill(), !pill_before);
    assert_eq!(status(), !status_before, "the status bar is left alone");

    // Hidden by the title bar's own menu already: the switch finds nothing to do.
    window.view.borrow_mut().toggle_format_pill();
    assert_eq!(pill(), pill_before);
    change(
        &fx,
        SettingsSection::Appearance,
        FieldId::ShowFormatPill,
        AccessAction::Toggle,
    );
    assert_eq!(pill(), pill_before);
    assert_eq!(
        fx.controller.settings().appearance.show_format_pill,
        pill_before
    );
}

// ----- the hub --------------------------------------------------------------------------------

#[test]
fn opening_the_window_twice_shows_the_same_one() {
    let fx = fixture(None);
    let first = fx.controller.create_settings(setup());
    let second = fx.controller.create_settings(setup());
    assert!(Rc::ptr_eq(&first, &second));
    assert!(Rc::ptr_eq(&first, &fx.controller.settings_cell().unwrap()));
}

#[test]
fn reports_without_a_window_and_cancelled_folder_pickers_change_nothing() {
    let fx = fixture(None);
    let before = fx.controller.settings().clone();
    fx.controller
        .settings_event(SettingsEvent::Changed(SettingsChange::Shortcuts));
    assert_eq!(*fx.controller.settings(), before);

    // The picker is shown, but no window is left to take the answer.
    fx.services.answer(
        DialogKind::PickFolder,
        Answer::Paths(vec![std::env::temp_dir()]),
    );
    fx.controller
        .settings_event(SettingsEvent::BrowseFolder(FieldId::DefaultFolder));
    assert_eq!(*fx.controller.settings(), before);

    // With the window open the user cancels the picker.
    let _cell = fx.controller.create_settings(setup());
    fx.controller
        .settings_event(SettingsEvent::BrowseFolder(FieldId::DefaultFolder));
    assert_eq!(*fx.controller.settings(), before);
    assert_eq!(fx.services.dialog_log().len(), 2);
}

#[test]
fn the_app_zoom_is_written_and_shown_in_an_open_settings_window() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    fx.controller.set_zoom(150);
    assert_eq!(
        cell.view
            .borrow()
            .window()
            .settings()
            .appearance
            .zoom_percent,
        150
    );
    fx.clock.advance(Duration::from_millis(401));
    fx.controller.pump_settings();
    assert_eq!(written(&fx).appearance.zoom_percent, 150);

    // Without the window the zoom is persisted just the same.
    fx.controller.close_settings();
    fx.controller.set_zoom(110);
    fx.clock.advance(Duration::from_millis(401));
    fx.controller.pump_settings();
    assert_eq!(written(&fx).appearance.zoom_percent, 110);
}

#[test]
fn an_existing_settings_file_is_opened_as_it_is() {
    let fx = fixture(Some(r#"{"editor":{"font_size":18}}"#));
    let before = std::fs::read_to_string(&fx.file).unwrap();
    fx.controller
        .settings_event(SettingsEvent::OpenSettingsFile);
    assert_eq!(std::fs::read_to_string(&fx.file).unwrap(), before);
    let window = fx.controller.windows()[0].clone();
    let open = window.view.borrow().tabs().iter().any(|tab| {
        tab.entry
            .path()
            .is_some_and(|path| path.ends_with("settings.json"))
    });
    assert!(open, "settings.json is open in a tab");
}

#[test]
fn without_a_window_the_settings_file_is_written_but_nothing_opens() {
    let fx = fixture(None);
    let id = fx.controller.windows()[0].id;
    fx.controller.close_window(id);
    assert!(fx.controller.active_window().is_none());
    fx.controller.open_settings_file();
    assert!(fx.file.exists());
    assert!(fx.controller.windows().is_empty());
}

#[test]
fn our_own_write_is_not_mistaken_for_a_hand_edit() {
    let fx = fixture(None);
    fx.controller.open_settings_file();
    let mut settings = fx.controller.settings().clone();
    settings.editor.font_size = 25.0;
    fx.controller.install_settings(settings, true);
    fx.clock.advance(Duration::from_millis(401));
    fx.controller.pump_settings();
    assert_eq!(written(&fx).editor.font_size, 25.0);

    // Not persisted: were the file we just wrote read back as an edit, this would be undone.
    let mut settings = fx.controller.settings().clone();
    settings.editor.font_size = 30.0;
    fx.controller.install_settings(settings, false);
    fx.controller.pump_settings();
    assert_eq!(font_size(&fx), 30.0);
}

#[test]
fn a_settings_file_that_disappears_is_ignored_until_it_is_back() {
    let fx = fixture(None);
    fx.controller.open_settings_file();
    let before = font_size(&fx);
    std::fs::remove_file(&fx.file).unwrap();
    fx.controller.pump_settings();
    fx.controller.pump_settings();
    assert_eq!(font_size(&fx), before);

    std::fs::write(&fx.file, r#"{"editor":{"font_size":23}}"#).unwrap();
    fx.controller.pump_settings();
    assert_eq!(font_size(&fx), 23.0);
}

#[test]
fn a_failed_write_is_reported_once_and_retried_after_thirty_seconds() {
    let fx = fixture(None);
    let i18n = localizer_for("en");
    let title = i18n.text("settingsapp-save-failed-title");
    // A folder where the file belongs: the write cannot succeed.
    std::fs::create_dir(&fx.file).unwrap();
    let _cell = fx.controller.create_settings(setup());
    change(
        &fx,
        SettingsSection::Editor,
        FieldId::FontSize,
        AccessAction::SetValue("20".to_owned()),
    );
    fx.clock.advance(Duration::from_millis(401));
    fx.controller.pump_settings();
    let log = fx.services.dialog_log();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].kind, DialogKind::MessageBox);
    assert_eq!(log[0].title, title);
    let reason = save_settings(&fx.file, &fx.controller.settings())
        .unwrap_err()
        .to_string();
    assert_eq!(
        log[0].message,
        i18n.format(
            "settingsapp-save-failed-message",
            &[("reason", Arg::Str(&reason))]
        )
    );

    // The next attempt is not before thirty seconds have passed.
    std::fs::remove_dir(&fx.file).unwrap();
    fx.clock.advance(Duration::from_secs(29));
    fx.controller.pump_settings();
    assert!(!fx.file.exists());

    // It fails again: the user is not told twice.
    std::fs::create_dir(&fx.file).unwrap();
    fx.clock.advance(Duration::from_secs(2));
    fx.controller.pump_settings();
    assert_eq!(fx.services.dialog_log().len(), 1);

    // Then it works.
    std::fs::remove_dir(&fx.file).unwrap();
    fx.clock.advance(Duration::from_secs(31));
    fx.controller.pump_settings();
    assert_eq!(written(&fx).editor.font_size, 20.0);

    // A later failure is announced again.
    std::fs::remove_file(&fx.file).unwrap();
    std::fs::create_dir(&fx.file).unwrap();
    change(
        &fx,
        SettingsSection::Editor,
        FieldId::FontSize,
        AccessAction::SetValue("21".to_owned()),
    );
    fx.clock.advance(Duration::from_millis(401));
    fx.controller.pump_settings();
    let log = fx.services.dialog_log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].title, title);
}

// ----- Debug ----------------------------------------------------------------------------------

#[test]
fn the_settings_types_describe_themselves_for_diagnostics() {
    let fx = fixture(None);
    let setup = setup();
    let text = format!("{setup:?}");
    assert!(text.starts_with("SettingsSetup {"), "{text}");
    assert!(text.contains("scale: 1.0"), "{text}");
    let cell = fx.controller.create_settings(setup);
    assert_eq!(format!("{cell:?}"), "SettingsCell { .. }");
    let text = format!("{:?}", cell.view.borrow());
    assert!(text.starts_with("SettingsView {"), "{text}");
    assert!(text.contains("scale: 1.0"), "{text}");
    assert!(text.contains("window: "), "{text}");
}
