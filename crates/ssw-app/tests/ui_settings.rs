//! The Settings window and the life of the settings: a change made in the window's controls
//! reaches every editor, the keymap, the theme and the spell checkers, is written to
//! `settings.json` after the debounce (unknown keys kept), and a hand edit of the file is read
//! back. Runs the real controller over a temporary profile with scripted dialogs and shell; the
//! window test at the end uses a real hidden native window.

use std::cell::RefCell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::i18n::localizer_for;
use ssw_app::startup::{Boot, ProcessEnv, boot};
use ssw_app::ui::controller::{Controller, Request};
use ssw_app::ui::host::{UiApp, UiOptions, UiServices};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices, ShellRecord};
use ssw_app::ui::settings_window::{
    SettingsSetup, WINDOW_SIZE, defaults_keeping_unknown, merge_changes, resolve_theme, window_rect,
};
use ssw_app::workers::DeferredWaker;
use ssw_core::command::CommandId;
use ssw_core::keymap::KeyChord;
use ssw_core::settings::{AccentMode, Settings, SettingsSection, Theme as ThemeSetting};
use ssw_io::paths::Environment;
use ssw_io::settings_file::load_settings;
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::spell::{SpellCheck, SpellProvider};
use ssw_ui::geometry::Color;
use ssw_ui::input::{KeyEvent, Modifiers};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult};
use ssw_ui::shell::settings::{DisplayKeyNames, FieldId, SettingsEvent};
use ssw_ui::shortcuts::{CharProbe, ShortcutOutcome};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme, ThemeMode};
use ssw_ui::widgets::SharedClipboard;
use ssw_ui::widgets::anim::Motion;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::theme::SystemTheme;
use ssw_win::window::{OsVersion, current_os_version, send_message};
use windows::Win32::UI::WindowsAndMessaging::{HMENU, WM_CLOSE};

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

fn editor_config(controller: &Controller) -> ssw_ui::editor::EditorConfig {
    let cell = controller.windows()[0].clone();
    let view = cell.view.borrow();
    view.tabs()[0].editor.config().clone()
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
        let _ = view.event(
            &ssw_ui::input::Event::AnimationFrame(Duration::ZERO),
            &engine,
        );
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

#[test]
fn a_change_in_the_window_reaches_the_editors_and_is_written_after_the_debounce() {
    let fx = fixture(Some(
        r#"{"future":{"x":1},"general":{"future_key":7},"editor":{"font_size":16}}"#,
    ));
    let _cell = fx.controller.create_settings(setup());
    assert_eq!(editor_config(&fx.controller).font_size, 16.0);

    change(
        &fx,
        SettingsSection::Editor,
        FieldId::FontSize,
        AccessAction::SetValue("20".to_owned()),
    );
    assert_eq!(fx.controller.settings().editor.font_size, 20.0);
    assert_eq!(editor_config(&fx.controller).font_size, 20.0);

    // Still the old file: the write waits for the debounce.
    fx.controller.pump_settings();
    assert!(
        std::fs::read_to_string(&fx.file)
            .unwrap()
            .contains("\"font_size\":16")
    );
    fx.clock.advance(Duration::from_millis(399));
    fx.controller.pump_settings();
    assert!(
        std::fs::read_to_string(&fx.file)
            .unwrap()
            .contains("\"font_size\":16")
    );
    fx.clock.advance(Duration::from_millis(2));
    fx.controller.pump_settings();

    let loaded = load_settings(&fx.file).unwrap().settings;
    assert_eq!(loaded.editor.font_size, 20.0);
    assert_eq!(loaded, *fx.controller.settings());
    assert_eq!(loaded.extra["future"]["x"], 1);
    assert_eq!(loaded.general.extra["future_key"], 7);
}

#[test]
fn closing_the_window_writes_a_pending_change_at_once() {
    let fx = fixture(None);
    let _cell = fx.controller.create_settings(setup());
    change(
        &fx,
        SettingsSection::Editor,
        FieldId::FontSize,
        AccessAction::SetValue("19".to_owned()),
    );
    assert!(!fx.file.exists());
    fx.controller.close_settings();
    assert!(fx.controller.settings_cell().is_none());
    assert_eq!(
        load_settings(&fx.file).unwrap().settings.editor.font_size,
        19.0
    );
}

#[test]
fn the_spell_checker_follows_the_setting() {
    struct Words;
    impl SpellProvider for Words {
        fn check(&self, _text: &str) -> Vec<Range<usize>> {
            Vec::new()
        }
        fn suggestions(&self, _word: &str) -> Vec<String> {
            Vec::new()
        }
        fn add_to_dictionary(&self, _word: &str) {}
        fn ignore(&self, _word: &str) {}
    }
    let fx = fixture(None);
    let check = Rc::new(SpellCheck::new(Rc::new(Words)));
    fx.controller.register_spell(&check);
    assert!(check.enabled());
    let _cell = fx.controller.create_settings(setup());
    change(
        &fx,
        SettingsSection::Editor,
        FieldId::SpellCheck,
        AccessAction::Toggle,
    );
    assert!(!check.enabled());
    assert!(!fx.controller.settings().editor.spell_check);
    change(
        &fx,
        SettingsSection::Editor,
        FieldId::SpellCheck,
        AccessAction::Toggle,
    );
    assert!(check.enabled());
}

#[test]
fn the_theme_follows_the_settings_over_the_system() {
    let fx = fixture(None);
    let system = SystemTheme {
        theme: Theme::light(DEFAULT_ACCENT),
        dark: false,
        high_contrast: false,
        transparency_effects: true,
        client_animations: true,
        accent: DEFAULT_ACCENT,
    };
    fx.controller.set_system_theme(system);
    let _cell = fx.controller.create_settings(setup());
    let mut settings = fx.controller.settings().clone();
    settings.appearance.theme = ThemeSetting::Dark;
    settings.appearance.accent = AccentMode::Custom;
    settings.appearance.accent_custom = Some("#112233".to_owned());
    fx.controller.install_settings(settings.clone(), true);
    let accent = Color::from_hex("#112233").unwrap();
    assert_eq!(fx.controller.theme(), Theme::dark(accent));
    assert_eq!(fx.controller.take_theme_change(), Some(Theme::dark(accent)));
    assert_eq!(fx.controller.take_theme_change(), None);
    assert_eq!(
        fx.controller.windows()[0].view.borrow().theme().mode,
        ThemeMode::Dark
    );
    // The system theme changing later does not undo the user's choice; high contrast wins.
    fx.controller.set_system_theme(SystemTheme {
        dark: false,
        ..system
    });
    assert_eq!(fx.controller.theme(), Theme::dark(accent));
    let high_contrast = SystemTheme {
        high_contrast: true,
        theme: Theme::dark(DEFAULT_ACCENT),
        ..system
    };
    assert_eq!(
        resolve_theme(&settings.appearance, &high_contrast),
        high_contrast.theme
    );
    settings.appearance.accent = AccentMode::Blue;
    settings.appearance.theme = ThemeSetting::System;
    assert_eq!(
        resolve_theme(&settings.appearance, &system),
        Theme::light(DEFAULT_ACCENT)
    );
    settings.appearance.accent = AccentMode::Custom;
    settings.appearance.accent_custom = Some("not a color".to_owned());
    assert_eq!(
        resolve_theme(&settings.appearance, &system),
        Theme::light(DEFAULT_ACCENT)
    );
}

#[test]
fn the_history_retention_reaches_the_autosave_scheduler() {
    let fx = fixture(None);
    let retention = || {
        fx.controller
            .core()
            .borrow()
            .autosave
            .policy()
            .history_retention_days
    };
    let mut settings = fx.controller.settings().clone();
    assert_eq!(retention(), settings.advanced.history_retention_days);
    settings.advanced.history_retention_days += 60;
    fx.controller.install_settings(settings.clone(), false);
    assert_eq!(retention(), settings.advanced.history_retention_days);
}

#[test]
fn shortcut_overrides_reach_the_router() {
    let fx = fixture(None);
    let mut settings = fx.controller.settings().clone();
    let chord = KeyChord {
        vk: u16::from(b'M'),
        ctrl: true,
        shift: false,
        alt: false,
        win: false,
    };
    settings
        .shortcuts
        .insert("file.new".to_owned(), vec![chord]);
    fx.controller.install_settings(settings, true);
    let outcome = fx.controller.route_key(
        &KeyEvent::press(u16::from(b'M'), Modifiers::CTRL),
        &NoAltGr,
        false,
    );
    assert_eq!(outcome, ShortcutOutcome::Command(CommandId::FileNew));
}

#[test]
fn reset_asks_first_keeps_unknown_keys_and_writes_the_file() {
    let fx = fixture(Some(
        r#"{"future":{"x":1},"editor":{"font_size":22,"newer":true}}"#,
    ));
    let _cell = fx.controller.create_settings(setup());
    fx.controller
        .install_settings(load_settings(&fx.file).unwrap().settings, false);
    assert_eq!(editor_config(&fx.controller).font_size, 22.0);

    fx.services
        .answer(DialogKind::MessageBox, Answer::Button(MessageBoxResult::No));
    fx.controller.settings_event(SettingsEvent::ResetSettings);
    assert_eq!(fx.controller.settings().editor.font_size, 22.0);

    fx.services.answer(
        DialogKind::MessageBox,
        Answer::Button(MessageBoxResult::Yes),
    );
    fx.controller.settings_event(SettingsEvent::ResetSettings);
    assert_eq!(fx.controller.settings().editor.font_size, 16.0);
    assert_eq!(editor_config(&fx.controller).font_size, 16.0);
    let loaded = load_settings(&fx.file).unwrap().settings;
    assert_eq!(loaded.editor.font_size, 16.0);
    assert_eq!(loaded.extra["future"]["x"], 1);
    assert_eq!(loaded.editor.extra["newer"], true);
    let cell = fx.controller.settings_cell().unwrap();
    assert_eq!(
        cell.view.borrow().window().settings().editor.font_size,
        16.0
    );
}

#[test]
fn settings_json_opens_in_a_tab_and_hand_edits_are_applied_without_being_overwritten() {
    let fx = fixture(None);
    fx.controller.open_settings_file();
    assert!(fx.file.exists());
    let tabs = fx.controller.windows()[0].view.borrow().tabs().len();
    assert!(tabs >= 1);
    let open = fx.controller.windows()[0]
        .view
        .borrow()
        .tabs()
        .iter()
        .any(|tab| {
            tab.entry
                .path()
                .is_some_and(|path| path.ends_with("settings.json"))
        });
    assert!(open, "settings.json is open in a tab");

    let edited = "{\"editor\":{\"font_size\":23}}";
    std::fs::write(&fx.file, edited).unwrap();
    fx.controller.pump_settings();
    assert_eq!(fx.controller.settings().editor.font_size, 23.0);
    assert_eq!(editor_config(&fx.controller).font_size, 23.0);
    // A file that does not parse yet is ignored until it does; nothing is written back.
    std::fs::write(&fx.file, "{\"editor\": {\"font_size\": 2").unwrap();
    fx.controller.pump_settings();
    assert_eq!(fx.controller.settings().editor.font_size, 23.0);
    fx.clock.advance(Duration::from_secs(5));
    fx.controller.pump_settings();
    assert_eq!(
        std::fs::read_to_string(&fx.file).unwrap(),
        "{\"editor\": {\"font_size\": 2"
    );
}

#[test]
fn browsing_a_folder_stores_the_chosen_path_and_the_logs_folder_opens_in_the_shell() {
    let fx = fixture(None);
    let _cell = fx.controller.create_settings(setup());
    let chosen = std::env::temp_dir();
    fx.services
        .answer(DialogKind::PickFolder, Answer::Paths(vec![chosen.clone()]));
    fx.controller
        .settings_event(SettingsEvent::BrowseFolder(FieldId::DefaultFolder));
    assert_eq!(
        fx.controller.settings().general.default_folder.as_deref(),
        Some(chosen.to_string_lossy().as_ref())
    );
    fx.controller.settings_event(SettingsEvent::OpenLogsFolder);
    assert!(matches!(
        fx.services.shell_log().last(),
        Some(ShellRecord::OpenPath(path)) if path.ends_with("logs")
    ));
}

#[test]
fn the_default_apps_button_asks_the_shell_for_the_settings_page_and_changes_nothing() {
    let fx = fixture(None);
    let _cell = fx.controller.create_settings(setup());
    let before = fx.controller.settings().clone();
    fx.controller.settings_event(SettingsEvent::OpenDefaultApps);
    assert_eq!(fx.services.shell_log(), [ShellRecord::OpenDefaultApps]);
    assert_eq!(*fx.controller.settings(), before);
}

#[test]
fn edits_are_merged_so_the_app_zoom_is_not_undone_by_a_stale_window() {
    let current = Settings {
        appearance: ssw_core::settings::Appearance {
            zoom_percent: 120,
            ..Default::default()
        },
        ..Default::default()
    };
    let base = Settings::default();
    let mut edited = base.clone();
    edited.editor.font_size = 20.0;
    edited.shortcuts.insert("file.new".to_owned(), Vec::new());
    let merged = merge_changes(&current, &base, &edited);
    assert_eq!(merged.appearance.zoom_percent, 120);
    assert_eq!(merged.editor.font_size, 20.0);
    assert!(merged.shortcuts.contains_key("file.new"));
    // Dropping an override in the window drops it in the merged settings too.
    let back = merge_changes(&merged, &edited, &base);
    assert!(!back.shortcuts.contains_key("file.new"));
    assert_eq!(back.appearance.zoom_percent, 120);

    let mut with_unknown = Settings::default();
    with_unknown
        .shortcuts
        .insert("from.the.future".to_owned(), Vec::new());
    with_unknown
        .shortcuts
        .insert("file.new".to_owned(), Vec::new());
    let fresh = defaults_keeping_unknown(&with_unknown);
    assert!(fresh.shortcuts.contains_key("from.the.future"));
    assert!(!fresh.shortcuts.contains_key("file.new"));
}

#[test]
fn the_window_opens_centred_over_its_owner() {
    assert_eq!(
        window_rect(Some((100, 50, 1400, 900)), 1.0),
        (350, 180, 900, 640)
    );
    assert_eq!(
        window_rect(Some((0, 0, 2000, 1200)), 1.5),
        (325, 120, 1350, 960)
    );
    assert_eq!(window_rect(None, 1.0), (100, 100, 900, 640));
}

#[test]
fn the_view_paints_its_title_and_pages_and_exposes_them_to_accessibility() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let engine = MonoTextEngine::default();
    let mut canvas = RecordingCanvas::new();
    cell.view.borrow_mut().paint(&mut canvas, &engine, false);
    assert!(canvas.is_balanced());
    let texts = canvas.texts();
    assert!(texts.contains(&"Settings"), "{texts:?}");
    assert!(texts.contains(&"Appearance"), "{texts:?}");
    let view = cell.view.borrow();
    let tree = view.access_tree(&engine);
    assert!(
        tree.nodes()
            .iter()
            .any(|node| node.role == Role::Window && node.name == "Settings")
    );
    assert_eq!(tree.root_id(), view.root_id());
}

// ----- the native window ---------------------------------------------------------------------

struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

struct Probe<F> {
    body: Option<F>,
}

impl<F: FnOnce(&UiApp)> ShellHost for Probe<F> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let _ole = OleGuard::initialize().unwrap();
        let services = ScriptedServices::new();
        let options = UiOptions {
            hidden: true,
            services: Some(UiServices {
                dialogs: services.dialogs(),
                shell: services.shell(),
                clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
                popups: Box::new(NoPopups),
            }),
            instance: None,
            jump_list: false,
            update_check: false,
            http: None,
            taskbar: None,
        };
        let app = UiApp::new(core, options).unwrap();
        app.pump();
        (self.body.take().unwrap())(&app);
        0
    }
}

#[test]
fn settings_open_in_one_native_window_that_closes_with_its_close_button() {
    let tmp = tempfile::tempdir().unwrap();
    let mut args = Args::default();
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Hardware,
        install_crash_handler: false,
    };
    let mut dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process,
        &mut *dialogs,
        None,
        &mut Probe {
            body: Some(|app: &UiApp| {
                assert!(app.settings_hwnd().is_none());
                app.controller().push(Request::OpenSettings);
                app.pump();
                let first = app.settings_hwnd().expect("the Settings window exists");
                // Opening again finds the same window.
                app.controller().push(Request::OpenSettings);
                app.pump();
                assert_eq!(app.settings_hwnd(), Some(first));
                assert!(app.controller().settings_cell().is_some());
                send_message(first, WM_CLOSE, 0, 0);
                app.pump();
                assert!(app.settings_hwnd().is_none());
                assert!(app.controller().settings_cell().is_none());
            }),
        },
    );
    assert_eq!(code, 0);
}
