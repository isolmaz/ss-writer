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
use ssw_ui::access::{AccessAction, AccessNode, Role};
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::editor::spell::{SpellCheck, SpellProvider};
use ssw_ui::geometry::{Color, Point};
use ssw_ui::input::{
    Event, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, TextInput,
};
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

/// The control labelled `field` on the page of `section` of the open Settings window.
fn control_node(fx: &Fixture, section: SettingsSection, field: FieldId) -> AccessNode {
    let cell = fx.controller.settings_cell().expect("settings are open");
    let engine = MonoTextEngine::default();
    let label = localizer_for("en").text(field.label_id());
    let mut view = cell.view.borrow_mut();
    view.window_mut().select_section(section);
    // One pass so the page of the section exists in the tree.
    let _ = view.event(&Event::AnimationFrame(Duration::ZERO), &engine);
    view.access_tree(&engine)
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
        .unwrap_or_else(|| panic!("no control named {label:?}"))
}

/// Delivers `event` to the open Settings window like its window procedure does and hands what
/// the window reports to the controller.
fn settings_input(fx: &Fixture, event: Event) {
    let cell = fx.controller.settings_cell().expect("settings are open");
    let events = cell
        .view
        .borrow_mut()
        .event(&event, &MonoTextEngine::default());
    for event in events {
        fx.controller.settings_event(event);
    }
}

fn click_settings(fx: &Fixture, at: Point) {
    for kind in [
        PointerEventKind::Move,
        PointerEventKind::Down,
        PointerEventKind::Up,
    ] {
        let button = (kind != PointerEventKind::Move).then_some(PointerButton::Left);
        settings_input(
            fx,
            Event::Pointer(PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, button, at)
            }),
        );
    }
}

/// Selects the text of the box under `at` (a click, Ctrl+A) and types `text` character by
/// character.
fn type_into(fx: &Fixture, at: Point, text: &str) {
    click_settings(fx, at);
    settings_input(fx, Event::Key(KeyEvent::press(0x41, Modifiers::CTRL)));
    for character in text.chars() {
        settings_input(fx, Event::Text(TextInput(character.to_string())));
    }
}

/// Changes the control labelled `field` of the open Settings window like a user would (through
/// its accessibility action) and hands the resulting events to the controller.
fn change(fx: &Fixture, section: SettingsSection, field: FieldId, action: AccessAction) {
    let cell = fx.controller.settings_cell().expect("settings are open");
    let engine = MonoTextEngine::default();
    let node = control_node(fx, section, field);
    let events = cell
        .view
        .borrow_mut()
        .perform_access(node.id, &action, &engine);
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

// ----- through the pointer and the keyboard -----------------------------------------------------

/// Opens `name` (a document with one line of text) in `cell` and returns the controller's view
/// of every open editor's configuration: `(window, tab)` in order.
fn open_document(fx: &Fixture, cell: &Rc<ssw_app::ui::controller::WindowCell>, name: &str) {
    let path = fx._tmp.path().join(name);
    std::fs::write(&path, "hello world\n").unwrap();
    fx.controller.open_file(cell, &path).unwrap();
}

fn all_configs(fx: &Fixture) -> Vec<ssw_ui::editor::EditorConfig> {
    fx.controller
        .windows()
        .iter()
        .flat_map(|cell| {
            let view = cell.view.borrow();
            view.tabs()
                .iter()
                .map(|tab| tab.editor.config().clone())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The size and origin of the line "hello world" the first window draws for its active tab.
fn drawn_line(fx: &Fixture) -> (ssw_ui::geometry::Size, Point) {
    let mut canvas = RecordingCanvas::new();
    let cell = fx.controller.windows()[0].clone();
    cell.view
        .borrow_mut()
        .paint(&mut canvas, &MonoTextEngine::default(), false);
    canvas
        .ops()
        .find_map(|op| match op {
            DrawOp::Text {
                text, size, origin, ..
            } if text.starts_with("hello world") => Some((*size, *origin)),
            _ => None,
        })
        .expect("the document's line is drawn")
}

#[test]
fn typing_editor_settings_with_the_mouse_and_keyboard_changes_every_open_editor_and_the_canvas() {
    let fx = fixture(Some(r#"{"general":{"autosave":false}}"#));
    let first = fx.controller.windows()[0].clone();
    open_document(&fx, &first, "one.md");
    open_document(&fx, &first, "two.md");
    let second = fx
        .controller
        .create_window(None, ssw_ui::geometry::Size::new(1000.0, 700.0), 1.0);
    open_document(&fx, &second, "three.md");
    let _cell = fx.controller.create_settings(setup());
    assert!(all_configs(&fx).len() >= 3);
    let before = drawn_line(&fx).0;
    assert_eq!(before.height, 16.0 * 1.75);

    let at = |field| {
        control_node(&fx, SettingsSection::Editor, field)
            .bounds
            .center()
    };
    type_into(&fx, at(FieldId::FontSize), "20");
    type_into(&fx, at(FieldId::LineHeight), "2");
    type_into(&fx, at(FieldId::TextFont), "Georgia");

    let stored = fx.controller.settings().editor.clone();
    assert_eq!(
        (
            stored.font_size,
            stored.line_height,
            stored.text_font.as_str()
        ),
        (20.0, 2.0, "Georgia")
    );
    for config in all_configs(&fx) {
        assert_eq!(config.font_size, 20.0);
        assert_eq!(config.line_height, 2.0);
        assert_eq!(&*config.text_font, "Georgia");
    }
    // What the window draws follows: the line is as tall as size × line height.
    assert_eq!(drawn_line(&fx).0.height, 20.0 * 2.0);

    // Closing the window writes what was typed.
    fx.controller.close_settings();
    let loaded = load_settings(&fx.file).unwrap().settings;
    assert_eq!(loaded.editor.font_size, 20.0);
    assert_eq!(loaded.editor.text_font, "Georgia");
}

#[test]
fn an_open_drop_down_of_the_settings_window_is_painted_last_and_takes_the_click() {
    let fx = fixture(None);
    let cell = fx.controller.create_settings(setup());
    let combo = control_node(&fx, SettingsSection::Editor, FieldId::ListIndent);
    click_settings(&fx, combo.bounds.center());

    let paint = |cell: &Rc<ssw_app::ui::settings_window::SettingsCell>| {
        let mut canvas = RecordingCanvas::new();
        cell.view
            .borrow_mut()
            .paint(&mut canvas, &MonoTextEngine::default(), false);
        canvas
    };
    // The last texts drawn are the options of the open list, not labels of the rows below it.
    let canvas = paint(&cell);
    let texts = canvas.texts();
    let last = &texts[texts.len() - 4..];
    assert_eq!(
        last,
        ["Align with the item text", "2 spaces", "4 spaces", "Tab"]
    );

    // The click on an option reaches the list, not the control under it.
    let tree = cell.view.borrow().access_tree(&MonoTextEngine::default());
    let option = tree
        .nodes()
        .iter()
        .find(|node| node.role == Role::ListItem && node.name == "4 spaces")
        .cloned()
        .unwrap();
    click_settings(&fx, option.bounds.center());
    let stored = fx.controller.settings().editor.clone();
    assert_eq!(format!("{:?}", stored.list_indent), "Four");
    assert_eq!(
        stored.typewriter_position,
        Settings::default().editor.typewriter_position
    );
}
