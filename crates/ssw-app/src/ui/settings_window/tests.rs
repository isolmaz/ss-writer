//! Pieces of the Settings window that need neither a controller nor a native window (how a
//! chord is named, the queue assistive technology hands its actions over in, the stamp of
//! `settings.json`, the hub's description) and the window procedure side over a real startup:
//! the handler is driven with a hand-made [`WindowInfo`], the native window is a hidden,
//! off-screen one that is never shown or activated. Input is made by calling the pieces (or
//! posting to the window), never by `SendInput`.

use ssw_core::keymap::KeyChord;
use ssw_core::settings::SettingsSection;
use ssw_io::net::WinHttp;
use ssw_io::paths::Environment;
use ssw_ui::access::{AccessNode, Role};
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::input::{KeyEvent, Modifiers, PointerEvent, PointerEventKind};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::settings::{DisplayKeyNames, SettingsChange};
use ssw_ui::widgets::SharedClipboard;
use ssw_win::dragdrop::OleGuard;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::Driver;
use ssw_win::taskbar::SystemTaskbars;
use ssw_win::window::{OsVersion, WM_WAKE, send_message};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{MSG, PM_REMOVE, PeekMessageW, WM_PAINT};

use super::*;
use crate::app::AppCore;
use crate::args::Args;
use crate::startup::{Boot, ProcessEnv, boot};
use crate::ui::scripted_services::ScriptedServices;
use crate::workers::DeferredWaker;

fn chord(key: u16, ctrl: bool, shift: bool, alt: bool, win: bool) -> KeyChord {
    KeyChord {
        vk: key,
        ctrl,
        shift,
        alt,
        win,
    }
}

/// What a keyboard layout names: Ctrl and S, nothing else.
fn some_names(key: u16) -> Option<String> {
    match key {
        vk::CONTROL => Some("Strg".to_owned()),
        0x53 => Some("S".to_owned()),
        _ => None,
    }
}

#[test]
fn a_chord_is_named_with_the_system_names_and_modifiers_without_one_are_spelled_in_english() {
    assert_eq!(
        chord_text_with(chord(0x53, true, true, true, true), some_names),
        "Strg+Shift+Alt+Win+S"
    );
    assert_eq!(
        chord_text_with(chord(0x53, false, false, false, false), some_names),
        "S"
    );
}

#[test]
fn a_chord_whose_key_has_no_name_is_shown_in_its_stored_form() {
    let unnamed = chord(0x07, true, false, false, false);
    let text = chord_text_with(unnamed, some_names);
    assert_eq!(text, unnamed.to_string());
    assert!(!text.contains("Strg"), "{text}");
}

#[test]
fn the_access_queue_keeps_the_actions_for_the_ui_thread_and_answers_at_once() {
    let queue = AccessQueue {
        items: Mutex::new(Vec::new()),
        wake: Mutex::new(None),
    };
    let id = WidgetId::allocate();
    assert!(queue.perform(id, AccessAction::Toggle));
    assert!(queue.perform(id, AccessAction::SetValue("x".to_owned())));
    let items = queue.items.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(items.len(), 2);
    assert!(items[0].0 == id && items[0].1 == AccessAction::Toggle);
    assert!(items[1].0 == id && items[1].1 == AccessAction::SetValue("x".to_owned()));
}

#[test]
fn a_stamp_is_empty_for_a_missing_file_and_holds_the_length_of_an_existing_one() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("settings.json");
    assert_eq!(stamp_of(&path), None);
    std::fs::write(&path, "{}").unwrap();
    let (_, length) = stamp_of(&path).expect("the file has a stamp");
    assert_eq!(length, 2);
}

#[test]
fn a_hub_without_a_window_describes_itself_as_closed_and_idle() {
    assert_eq!(
        format!("{:?}", SettingsHub::default()),
        "SettingsHub { open: false, due: None, .. }"
    );
}

// ----- the harness ----------------------------------------------------------------------------

/// A keyboard layout on which Ctrl+Alt+key never types a character.
struct NoAltGr;

impl CharProbe for NoAltGr {
    fn produces_char(&self, _vk: u16, _scan: u16, _shift: bool, _caps: bool) -> bool {
        false
    }
}

/// A startup in a temporary profile with what a Settings window needs: the controller, the
/// window environment, the graphics devices and the imaging factory.
struct Rig {
    controller: Rc<Controller>,
    env: Rc<WindowEnv>,
    devices: GfxDevices,
    wic: Wic,
    logger: Arc<Logger>,
    _profile: tempfile::TempDir,
}

fn rig() -> Rig {
    let profile = tempfile::tempdir().unwrap();
    let root = profile.path();
    std::fs::create_dir_all(root.join("roaming").join("SS Writer")).unwrap();
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
    let mut core = match boot(&Args::default(), process, &mut *dialogs, None).unwrap() {
        Boot::Ready(started) => AppCore::new(*started, Arc::new(DeferredWaker::new())),
        Boot::Exit(code) => panic!("startup exited with {code}"),
    };
    let devices = core.wait_devices().unwrap();
    let logger = Arc::clone(&core.logger);
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
    let controller = Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
    while controller.take_request().is_some() {}
    let env = Rc::new(WindowEnv {
        controller: Rc::clone(&controller),
        text: DWriteEngine::new().unwrap(),
        probe: Box::new(NoAltGr),
        mica: Cell::new(true),
        http: Arc::new(WinHttp),
        taskbar: Rc::new(SystemTaskbars),
    });
    Rig {
        controller,
        env,
        devices,
        wic: Wic::new().unwrap(),
        logger,
        _profile: profile,
    }
}

/// Runs `body` over a startup; the imaging objects live until `body` and everything it made
/// are gone.
fn with_rig(body: impl FnOnce(&Rig)) {
    // Declared first: dropped after every COM object of the windows.
    let _ole = OleGuard::initialize().unwrap();
    let rig = rig();
    body(&rig);
}

fn open_cell(rig: &Rig) -> Rc<SettingsCell> {
    rig.controller.create_settings(SettingsSetup {
        probe: Box::new(NoAltGr),
        key_names: Box::new(DisplayKeyNames),
        motion: Motion::reduced(),
        size: WINDOW_SIZE,
        scale: 1.0,
    })
}

/// The native Settings window of `cell`: hidden, off-screen, never activated.
fn hidden_window(rig: &Rig, cell: &Rc<SettingsCell>) -> SettingsNative {
    SettingsNative::create(
        &rig.env,
        &rig.devices,
        &rig.wic,
        &rig.logger,
        Rc::clone(cell),
        (-32000, -32000, 900, 640),
        true,
    )
    .expect("the hidden Settings window")
}

/// The control labelled `field` on the page of `section`, after the page was laid out.
fn control(rig: &Rig, cell: &SettingsCell, section: SettingsSection, field: FieldId) -> AccessNode {
    let label = localizer_for("en").text(field.label_id());
    let mut view = cell.view.borrow_mut();
    view.window_mut().select_section(section);
    // One pass so the page of the section exists in the tree.
    let _ = view.event(&Event::AnimationFrame(Duration::ZERO), &rig.env.text);
    view.access_tree(&rig.env.text)
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

/// Gives the language drop-down the keyboard focus.
fn focus_language(rig: &Rig, cell: &SettingsCell) {
    let node = control(rig, cell, SettingsSection::General, FieldId::Language);
    assert_eq!(node.role, Role::ComboBox);
    let events =
        cell.view
            .borrow_mut()
            .perform_access(node.id, &AccessAction::Focus, &rig.env.text);
    assert!(events.is_empty());
}

fn window_info() -> WindowInfo {
    WindowInfo {
        hwnd: HWND::default(),
        dpi: 96,
        size_px: (900, 640),
        maximized: false,
        active: true,
    }
}

fn dark_system() -> SystemTheme {
    SystemTheme {
        theme: Theme::dark(DEFAULT_ACCENT),
        dark: true,
        high_contrast: false,
        transparency_effects: true,
        client_animations: true,
        accent: DEFAULT_ACCENT,
    }
}

/// The handler of a Settings window whose native window is never made.
struct GlueRig {
    glue: SettingsGlue,
    cell: Rc<SettingsCell>,
    access: Rc<SettingsAccess>,
    link: Rc<SettingsLink>,
}

fn glue_of(rig: &Rig) -> GlueRig {
    let cell = open_cell(rig);
    let access = Rc::new(SettingsAccess {
        queue: Arc::new(AccessQueue {
            items: Mutex::new(Vec::new()),
            wake: Mutex::new(None),
        }),
        bridge: RefCell::new(None),
        active: Cell::new(false),
    });
    let link = Rc::new(SettingsLink::default());
    let glue = SettingsGlue {
        env: Rc::clone(&rig.env),
        cell: Rc::clone(&cell),
        link: Rc::clone(&link),
        access: Rc::clone(&access),
    };
    GlueRig {
        glue,
        cell,
        access,
        link,
    }
}

/// The tree the bridge of `access` last received.
fn published(access: &SettingsAccess) -> Arc<AccessTree> {
    access
        .bridge
        .borrow()
        .as_ref()
        .expect("the bridge exists")
        .tree()
}

/// Whether a wake-up was posted to the window `hwnd` (the message is taken out of the queue).
fn take_wake(hwnd: HWND) -> bool {
    let mut message = MSG::default();
    // SAFETY: a plain peek into the queue of a window of this thread, with a valid MSG.
    unsafe { PeekMessageW(&mut message, Some(hwnd), WM_WAKE, WM_WAKE, PM_REMOVE) }.as_bool()
}

// ----- the window procedure side --------------------------------------------------------------

#[test]
fn the_caption_strip_belongs_to_the_system_and_closing_is_left_to_the_loop_owner() {
    with_rig(|rig| {
        let mut fx = glue_of(rig);
        let info = window_info();
        let at = |x: f32, y: f32| fx.glue.hit_test(Point::new(x, y), &info);
        assert_eq!(at(880.0, 10.0), HitRegion::Close);
        assert_eq!(at(830.0, 10.0), HitRegion::Maximize);
        assert_eq!(at(780.0, 10.0), HitRegion::Minimize);
        assert_eq!(at(300.0, 10.0), HitRegion::Caption);
        assert_eq!(at(300.0, 100.0), HitRegion::Client);

        // The handler never closes the window itself: it asks the loop owner to.
        assert!(!fx.glue.on_close_requested(&info));
        assert_eq!(rig.controller.take_request(), Some(Request::CloseSettings));
        assert!(!fx.link.destroyed.get());
        fx.glue.on_destroy(&info);
        assert!(fx.link.destroyed.get());
    });
}

#[test]
fn what_the_user_does_becomes_a_request_and_a_borrowed_view_drops_the_input() {
    with_rig(|rig| {
        let mut fx = glue_of(rig);
        let info = window_info();
        focus_language(rig, &fx.cell);
        while rig.controller.take_request().is_some() {}
        let down = || Event::Key(KeyEvent::press(vk::DOWN, Modifiers::NONE));
        let language = |cell: &SettingsCell| {
            cell.view
                .borrow()
                .window()
                .settings()
                .general
                .language
                .clone()
        };
        let before = language(&fx.cell);

        // A view that is borrowed (a modal dialog pumps messages) loses the key.
        let held = fx.cell.view.borrow_mut();
        fx.glue.on_event(&info, down());
        drop(held);
        assert_eq!(rig.controller.take_request(), None);
        assert_eq!(language(&fx.cell), before);

        // Free again, the key picks the next language and the change is queued.
        fx.glue.on_event(&info, down());
        assert_eq!(
            rig.controller.take_request(),
            Some(Request::Settings(SettingsEvent::Changed(
                SettingsChange::Field(FieldId::Language)
            )))
        );
        assert_eq!(rig.controller.take_request(), None);
        assert_ne!(language(&fx.cell), before);
    });
}

#[test]
fn painting_follows_the_size_and_the_backdrop_and_skips_a_borrowed_view() {
    with_rig(|rig| {
        let mut fx = glue_of(rig);
        let info = window_info();
        let paint = |glue: &mut SettingsGlue| {
            let mut canvas = RecordingCanvas::new();
            glue.on_paint(&info, &mut canvas);
            canvas
        };
        let fills = |canvas: &RecordingCanvas, wanted: Rect| {
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { rect, .. } if *rect == wanted))
        };
        let strip = |width: f32| Rect::new(0.0, 0.0, width, CAPTION_HEIGHT);

        // With the backdrop the caption strip is left to it.
        assert!(!fills(&paint(&mut fx.glue), strip(900.0)));
        // The window turned the transparency off, or the settings did.
        rig.env.mica.set(false);
        assert!(fills(&paint(&mut fx.glue), strip(900.0)));
        rig.env.mica.set(true);
        let mut settings = rig.controller.settings().clone();
        settings.appearance.mica = false;
        rig.controller.install_settings(settings, false);
        assert!(fills(&paint(&mut fx.glue), strip(900.0)));

        // A new size is laid out and painted; painting answers the repaint request.
        let mut larger = info;
        larger.size_px = (1000, 700);
        fx.glue.on_resize(&larger);
        let canvas = paint(&mut fx.glue);
        assert!(fills(&canvas, strip(1000.0)));
        assert!(!fills(&canvas, strip(900.0)));
        assert!(
            fx.cell
                .view
                .borrow_mut()
                .requests_mut()
                .take_invalidation()
                .is_clean()
        );

        // A borrowed view is neither resized nor painted.
        let mut smaller = info;
        smaller.size_px = (800, 600);
        let held = fx.cell.view.borrow_mut();
        fx.glue.on_resize(&smaller);
        assert_eq!(paint(&mut fx.glue).ops().count(), 0);
        drop(held);
        assert!(fills(&paint(&mut fx.glue), strip(1000.0)));
    });
}

#[test]
fn a_new_system_theme_reaches_the_controller_and_the_windows_follow_it() {
    with_rig(|rig| {
        let mut fx = glue_of(rig);
        assert_eq!(rig.controller.take_theme_change(), None);
        fx.glue.on_theme_changed(&window_info(), &dark_system());
        assert_eq!(rig.controller.theme().mode, ThemeMode::Dark);
        let followed = rig.controller.take_theme_change().map(|theme| theme.mode);
        assert_eq!(followed, Some(ThemeMode::Dark));
    });
}

#[test]
fn the_first_object_request_publishes_the_tree_unless_the_view_is_borrowed() {
    with_rig(|rig| {
        let mut fx = glue_of(rig);
        let info = window_info();
        let root = fx.cell.view.borrow().root_id();
        let ask =
            |glue: &mut SettingsGlue| glue.on_message(&info, WM_GETOBJECT, WPARAM(0), LPARAM(1));

        // Other messages are none of its business.
        assert_eq!(
            fx.glue.on_message(&info, WM_PAINT, WPARAM(0), LPARAM(0)),
            None
        );
        assert!(!fx.access.active.get());
        // Without a bridge the request is noted and nobody answers it.
        assert_eq!(ask(&mut fx.glue), None);
        assert!(fx.access.active.get());

        let queue = Arc::clone(&fx.access.queue) as Arc<dyn ActionSink>;
        *fx.access.bridge.borrow_mut() = Some(UiaBridge::new(HWND::default(), queue, root));
        let empty = published(&fx.access);

        // The view is borrowed when the client asks: nothing is published.
        fx.access.active.set(false);
        let held = fx.cell.view.borrow_mut();
        assert_eq!(ask(&mut fx.glue), None);
        drop(held);
        assert!(Arc::ptr_eq(&empty, &published(&fx.access)));

        // Free, the first request publishes the tree of the window.
        fx.access.active.set(false);
        assert_eq!(ask(&mut fx.glue), None);
        let tree = published(&fx.access);
        assert!(!Arc::ptr_eq(&empty, &tree));
        assert_eq!(tree.root_id(), root);
        assert!(
            tree.nodes()
                .iter()
                .any(|node| node.role == Role::Window && node.name == "Settings")
        );

        // Later requests do not publish again: the loop owner keeps the tree up to date.
        fx.cell
            .view
            .borrow_mut()
            .window_mut()
            .select_section(SettingsSection::Editor);
        assert_eq!(ask(&mut fx.glue), None);
        assert!(Arc::ptr_eq(&tree, &published(&fx.access)));
    });
}

// ----- the native window ----------------------------------------------------------------------

#[test]
fn assistive_technology_acts_through_the_queue_and_the_changed_tree_is_published() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let native = hidden_window(rig, &cell);
        let hwnd = native.hwnd();
        let toggle = control(
            rig,
            &cell,
            SettingsSection::Appearance,
            FieldId::ShowStatusBar,
        );
        let flag = || {
            cell.view
                .borrow()
                .window()
                .settings()
                .appearance
                .show_status_bar
        };
        let before = flag();

        // A repaint request is answered by painting the window.
        cell.view.borrow_mut().requests_mut().invalidate_all();
        let _ = send_message(hwnd, WM_PAINT, 0, 0);
        assert!(
            cell.view
                .borrow_mut()
                .requests_mut()
                .take_invalidation()
                .is_clean()
        );

        // Nothing is published before a client asks for the window object.
        assert!(!native.access.active.get());
        let _ = send_message(hwnd, WM_GETOBJECT, 0, 1);
        assert!(native.access.active.get());
        let tree = published(&native.access);
        assert_eq!(tree.root_id(), cell.view.borrow().root_id());
        assert_eq!(
            tree.node(toggle.id).map(|node| node.state.toggle),
            Some(toggle.state.toggle)
        );

        // The client toggles the switch on its own thread: the action waits for the UI thread
        // and wakes it up.
        assert!(native.access.queue.perform(toggle.id, AccessAction::Toggle));
        assert!(take_wake(hwnd), "the loop owner is woken");
        assert_eq!(
            flag(),
            before,
            "nothing changed before the UI thread ran it"
        );
        let events = native.sync();
        assert_eq!(
            events,
            [SettingsEvent::Changed(SettingsChange::Field(
                FieldId::ShowStatusBar
            ))]
        );
        assert_eq!(flag(), !before);

        // The tree the client sees follows, and the next turn has nothing left to do.
        let tree = published(&native.access);
        assert_ne!(
            tree.node(toggle.id).map(|node| node.state.toggle),
            Some(toggle.state.toggle)
        );
        assert!(native.sync().is_empty());
    });
}

#[test]
fn a_destroyed_window_runs_no_more_actions() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let native = hidden_window(rig, &cell);
        let toggle = control(
            rig,
            &cell,
            SettingsSection::Appearance,
            FieldId::ShowStatusBar,
        );
        // A client had been connected: destroying the window disconnects its provider.
        let _ = send_message(native.hwnd(), WM_GETOBJECT, 0, 1);
        assert!(!native.destroyed());
        native.host.destroy();
        assert!(native.destroyed());

        let before = cell.view.borrow().window().settings().clone();
        assert!(native.access.queue.perform(toggle.id, AccessAction::Toggle));
        assert!(native.sync().is_empty());
        assert_eq!(*cell.view.borrow().window().settings(), before);
    });
}

#[test]
fn a_borrowed_view_is_neither_synced_nor_published_nor_painted() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let native = hidden_window(rig, &cell);
        let hwnd = native.hwnd();
        let _ = control(
            rig,
            &cell,
            SettingsSection::Appearance,
            FieldId::ShowStatusBar,
        );
        let _ = send_message(hwnd, WM_GETOBJECT, 0, 1);
        let before = published(&native.access);

        native.access.active.set(false);
        let held = cell.view.borrow_mut();
        let _ = send_message(hwnd, WM_PAINT, 0, 0);
        let _ = send_message(hwnd, WM_GETOBJECT, 0, 1);
        native.access.publish(&cell, &rig.env.text);
        assert!(native.sync().is_empty());
        drop(held);
        assert!(native.access.active.get());
        assert!(Arc::ptr_eq(&before, &published(&native.access)));
    });
}

#[test]
fn the_cursor_a_widget_asks_for_is_kept_for_the_window() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let native = hidden_window(rig, &cell);
        let field = control(rig, &cell, SettingsSection::Editor, FieldId::FontSize);
        let middle = Point::new(
            field.bounds.x + field.bounds.width / 2.0,
            field.bounds.y + field.bounds.height / 2.0,
        );
        let moved = Event::Pointer(PointerEvent::mouse(PointerEventKind::Move, None, middle));
        let events = cell.view.borrow_mut().event(&moved, &rig.env.text);
        assert!(events.is_empty());
        assert_eq!(cell.cursor.get(), Cursor::Arrow);

        assert!(native.sync().is_empty());
        assert_eq!(cell.cursor.get(), Cursor::IBeam);
        // It stays until a widget asks for another one.
        assert!(native.sync().is_empty());
        assert_eq!(cell.cursor.get(), Cursor::IBeam);
    });
}

#[test]
fn the_window_follows_the_theme_the_controller_switched_to_with_one_repaint() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let native = hidden_window(rig, &cell);
        // A new window is painted for the first time; the second turn finds nothing to do.
        assert!(native.sync().is_empty());
        assert!(native.sync().is_empty());
        assert!(!cell.dirty.get());

        rig.controller.set_system_theme(dark_system());
        let theme = rig
            .controller
            .take_theme_change()
            .expect("the controller switched themes");
        assert_eq!(theme.mode, ThemeMode::Dark);
        native.apply_theme(theme);
        assert!(
            cell.dirty.get(),
            "the window is to be repainted in the new theme"
        );
        // The theme switch itself invalidated the view too: the second turn settles the flag.
        assert!(native.sync().is_empty());
        assert!(native.sync().is_empty());
        assert!(!cell.dirty.get(), "the repaint was asked for");
    });
}

#[test]
fn the_native_window_describes_itself_with_its_host() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let native = hidden_window(rig, &cell);
        let text = format!("{native:?}");
        assert!(text.starts_with("SettingsNative { host: "), "{text}");
        assert!(text.ends_with(", .. }"), "{text}");
    });
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_piece_of_the_native_window_that_cannot_be_made_is_reported() {
    with_rig(|rig| {
        let cell = open_cell(rig);
        let outcomes = ssw_win::fault::sweep(|| {
            SettingsNative::create(
                &rig.env,
                &rig.devices,
                &rig.wic,
                &rig.logger,
                Rc::clone(&cell),
                (-32000, -32000, 900, 640),
                true,
            )
            .err()
            .map(|error| error.to_string())
        });
        let failed: Vec<&str> = outcomes.iter().flatten().map(String::as_str).collect();
        assert!(failed.len() >= 3, "{outcomes:?}");
        // The window, its scene renderer and its composition surface fail in their own steps.
        for step in [
            "CreateWindowExW",
            "create DirectWrite factory",
            "DXGI adapter",
        ] {
            assert!(
                failed.iter().any(|message| message.contains(step)),
                "{step}: {failed:?}"
            );
        }
    });
}
