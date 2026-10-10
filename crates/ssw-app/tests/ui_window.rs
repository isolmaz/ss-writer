//! The native shell window: a real hidden window per `UiApp` window, driven with messages sent
//! to our own window (never `SendInput`), observed through the document text, the bytes on
//! disk, the exit decision and the view state. Windows are never shown or activated.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::Request;
use ssw_app::ui::host::{UiApp, UiOptions, UiServices};
use ssw_app::ui::menus::menu_id;
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::shell_view::ShellAction;
use ssw_core::command::CommandId;
use ssw_core::text::TextRange;
use ssw_io::net::{Http, ScriptedHttp};
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::blocks_media::MediaEvent;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Modifiers, PointerEvent, PointerEventKind, PointerType};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::shell::titlebar::{MenuKind, TitleEvent};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dmanip::wheel_scroll_lines;
use ssw_win::dragdrop::OleGuard;
use ssw_win::gfx::Driver;
use ssw_win::menus::{PopupHost, PopupRequest};
use ssw_win::taskbar::{Progress, TaskbarApi, TaskbarFactory, taskbar_button_created_message};
use ssw_win::theme::{Win32Settings, read_system_theme};
use ssw_win::wic::{DecodedImage, Wic};
use ssw_win::window::{current_os_version, send_message};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::Shell::{TBPF_NOPROGRESS, TBPF_NORMAL, TBPFLAG};
use windows::Win32::UI::WindowsAndMessaging::{
    DM_POINTERHITTEST, GetMenuItemCount, GetMenuItemID, GetMenuStringW, GetWindowRect, HMENU,
    MF_BYPOSITION, WM_CHAR, WM_CLOSE, WM_DPICHANGED, WM_KEYDOWN, WM_MOUSEWHEEL, WM_PAINT,
    WM_POINTERDOWN, WM_RBUTTONDOWN, WM_SETTINGCHANGE,
};

/// The key code of End.
const VK_END: usize = 0x23;

/// Menus are never shown in these tests.
struct NoPopups;

impl PopupHost for NoPopups {
    fn track(&mut self, _menu: HMENU, _request: &PopupRequest) -> Option<u32> {
        None
    }
}

/// What the scripted popup host saw and what it answers.
#[derive(Debug, Default)]
struct PopupLog {
    /// The labels of the top-level items of every popup shown (separators are empty).
    shown: Vec<Vec<(u32, String)>>,
    /// The screen position of every popup shown.
    positions: Vec<(i32, i32)>,
    /// The id the next popup returns (`None`: the user dismisses it).
    choose: Option<u32>,
}

/// A popup host that reads the native menu it is given and answers like a user.
struct ScriptedPopups(Rc<RefCell<PopupLog>>);

impl PopupHost for ScriptedPopups {
    fn track(&mut self, menu: HMENU, request: &PopupRequest) -> Option<u32> {
        // SAFETY: `menu` is the live menu `PopupMenus::show` built for this call.
        let count = unsafe { GetMenuItemCount(Some(menu)) };
        let mut items = Vec::new();
        for position in 0..count {
            // SAFETY: `position` is below the item count of the live `menu`.
            let id = unsafe { GetMenuItemID(menu, position) };
            let mut label = [0u16; 128];
            // SAFETY: `label` is a valid buffer for the call; the item is addressed by position.
            let length =
                unsafe { GetMenuStringW(menu, position as u32, Some(&mut label), MF_BYPOSITION) };
            let length = usize::try_from(length).unwrap_or(0);
            items.push((id, String::from_utf16_lossy(&label[..length])));
        }
        let mut log = self.0.borrow_mut();
        log.shown.push(items);
        log.positions.push((request.x, request.y));
        log.choose.take()
    }
}

/// Runs `body` inside the shell host of a real startup over a temporary profile, with the
/// file `notes.md` (containing `text`) opened in a hidden window.
struct Probe<F> {
    body: Option<F>,
    services: ScriptedServices,
    seams: Seams,
}

/// What a test substitutes for the real thing: the popup menus, the network, the taskbar.
#[derive(Default)]
struct Seams {
    popups: Option<ScriptedPopups>,
    http: Option<Arc<dyn Http>>,
    taskbar: Option<Rc<dyn TaskbarFactory>>,
}

impl<F: FnOnce(&UiApp, &Path)> ShellHost for Probe<F> {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        // Declared first: dropped after every COM object of the window.
        let _ole = OleGuard::initialize().unwrap();
        let services = self.services.clone();
        let seams = std::mem::take(&mut self.seams);
        let options = UiOptions {
            hidden: true,
            services: Some(UiServices {
                dialogs: services.dialogs(),
                shell: services.shell(),
                clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
                popups: match seams.popups {
                    Some(popups) => Box::new(popups),
                    None => Box::new(NoPopups),
                },
            }),
            instance: None,
            jump_list: false,
            http: seams.http,
            taskbar: seams.taskbar,
        };
        let app = UiApp::new(core, options).unwrap();
        app.pump();
        let file = args.paths[0].clone();
        (self.body.take().unwrap())(&app, &file);
        0
    }
}

fn with_window(text: &str, body: impl FnOnce(&UiApp, &Path)) {
    with_window_using(text, None, &ScriptedServices::new(), body);
}

/// [`with_window`] with a `settings.json` in the profile and scripted dialog answers.
fn with_window_using(
    text: &str,
    settings: Option<&str>,
    services: &ScriptedServices,
    body: impl FnOnce(&UiApp, &Path),
) {
    launch(text, settings, services, Seams::default(), body);
}

/// [`with_window`] whose popup menus are shown by the scripted host writing to `popups`.
fn with_popups(text: &str, popups: &Rc<RefCell<PopupLog>>, body: impl FnOnce(&UiApp, &Path)) {
    let seams = Seams {
        popups: Some(ScriptedPopups(Rc::clone(popups))),
        ..Seams::default()
    };
    launch(text, None, &ScriptedServices::new(), seams, body);
}

/// [`with_window`] over a `settings.json` and the given substitutes.
fn with_seams(text: &str, settings: Option<&str>, seams: Seams, body: impl FnOnce(&UiApp, &Path)) {
    launch(text, settings, &ScriptedServices::new(), seams, body);
}

fn launch(
    text: &str,
    settings: Option<&str>,
    services: &ScriptedServices,
    seams: Seams,
    body: impl FnOnce(&UiApp, &Path),
) {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("notes.md");
    std::fs::write(&file, text).unwrap();
    if let Some(settings) = settings {
        let folder = tmp.path().join("profile").join("roaming").join("SS Writer");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("settings.json"), settings).unwrap();
    }
    let mut args = Args {
        paths: vec![file],
        ..Args::default()
    };
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
            body: Some(body),
            services: services.clone(),
            seams,
        },
    );
    assert_eq!(code, 0);
}

#[test]
fn closing_with_unsaved_changes_asks_and_the_answer_decides() {
    use ssw_app::ui::scripted_services::{Answer, DialogKind};
    use ssw_ui::services::MessageBoxResult;
    let services = ScriptedServices::new();
    with_window_using(
        "hello",
        Some(r#"{"general":{"autosave":false}}"#),
        &services,
        |app, file| {
            let hwnd = first_window_hwnd(app);
            send_message(hwnd, WM_KEYDOWN, VK_END, 0);
            send_message(hwnd, WM_CHAR, usize::from(b'!'), 0);
            app.pump();
            // Cancel: the window stays, nothing is written.
            services.answer(
                DialogKind::MessageBox,
                Answer::Button(MessageBoxResult::Cancel),
            );
            send_message(hwnd, WM_CLOSE, 0, 0);
            app.pump();
            assert_eq!(app.exit_code(), None);
            assert_eq!(app.controller().windows().len(), 1);
            assert_eq!(std::fs::read_to_string(file).unwrap(), "hello");
            // Don't save: the application leaves and the file keeps its old bytes.
            services.answer(
                DialogKind::MessageBox,
                Answer::Button(MessageBoxResult::DontSave),
            );
            send_message(hwnd, WM_CLOSE, 0, 0);
            app.pump();
            assert_eq!(app.exit_code(), Some(0));
            assert_eq!(std::fs::read_to_string(file).unwrap(), "hello");
            let asked = services.dialog_log();
            assert_eq!(asked.len(), 2);
            assert!(
                asked
                    .iter()
                    .all(|dialog| dialog.kind == DialogKind::MessageBox && dialog.answered)
            );
        },
    );
}

fn first_window_hwnd(app: &UiApp) -> windows::Win32::Foundation::HWND {
    let id = app.controller().windows()[0].id;
    app.hwnd(id).expect("the native window exists")
}

fn document_text(app: &UiApp) -> String {
    let cell = app.controller().windows()[0].clone();
    let view = cell.view.borrow();
    view.active_tab()
        .expect("a tab is open")
        .entry
        .document()
        .borrow()
        .text()
}

#[test]
fn keys_and_characters_sent_to_the_window_edit_the_document() {
    with_window("hello", |app, _| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_KEYDOWN, VK_END, 0);
        for ch in "!?".chars() {
            send_message(hwnd, WM_CHAR, ch as usize, 0);
        }
        app.pump();
        assert_eq!(document_text(app), "hello!?");
    });
}

#[test]
fn closing_the_last_window_saves_the_document_and_quits() {
    with_window("hello", |app, file| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_KEYDOWN, VK_END, 0);
        send_message(hwnd, WM_CHAR, usize::from(b'!'), 0);
        app.pump();
        assert_eq!(app.exit_code(), None);
        // The close button: the window procedure only asks, the loop negotiates.
        send_message(hwnd, WM_CLOSE, 0, 0);
        assert!(app.hwnd(app.controller().windows()[0].id).is_some());
        app.pump();
        assert_eq!(app.exit_code(), Some(0));
        assert!(app.controller().windows().is_empty());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "hello!");
        assert!(app.final_session().is_some());
    });
}

#[test]
fn a_system_theme_change_message_applies_the_system_theme() {
    with_window("hello", |app, _| {
        let hwnd = first_window_hwnd(app);
        let system = read_system_theme(&Win32Settings).theme;
        // Start from a theme the system does not have.
        let other = if system == ssw_ui::theme::Theme::dark(ssw_ui::theme::DEFAULT_ACCENT) {
            ssw_ui::theme::Theme::light(ssw_ui::theme::DEFAULT_ACCENT)
        } else {
            ssw_ui::theme::Theme::dark(ssw_ui::theme::DEFAULT_ACCENT)
        };
        app.controller().set_theme(other);
        assert_ne!(app.controller().theme(), system);
        let setting: Vec<u16> = "ImmersiveColorSet\0".encode_utf16().collect();
        send_message(hwnd, WM_SETTINGCHANGE, 0, setting.as_ptr() as isize);
        assert_eq!(app.controller().theme(), system);
        let window = app.controller().windows()[0].clone();
        assert_eq!(*window.view.borrow().theme(), system);
    });
}

#[test]
fn a_dpi_change_rescales_the_view_and_resizes_it() {
    with_window("hello", |app, _| {
        let hwnd = first_window_hwnd(app);
        let window = app.controller().windows()[0].clone();
        let dpi = 144usize;
        // The suggested rectangle (left, top, right, bottom) of the new DPI.
        let suggested = [20i32, 20, 1620, 1020];
        send_message(
            hwnd,
            WM_DPICHANGED,
            (dpi << 16) | dpi,
            suggested.as_ptr() as isize,
        );
        let view = window.view.borrow();
        assert!((view.layout().scale() - 1.5).abs() < 1e-3);
        let size = view.layout().size();
        // 1600 px wide outer window: about 1067 DIPs minus the frame.
        assert!(size.width > 900.0 && size.width < 1100.0, "{size:?}");
    });
}

fn selection_range(app: &UiApp) -> (usize, usize) {
    let cell = app.controller().windows()[0].clone();
    let view = cell.view.borrow();
    let selection = view
        .active_tab()
        .expect("a tab is open")
        .entry
        .document()
        .borrow()
        .primary_selection();
    (
        selection.anchor.min(selection.head),
        selection.anchor.max(selection.head),
    )
}

#[test]
fn a_right_click_in_the_editor_shows_the_edit_popup_and_runs_the_choice() {
    let log = Rc::new(RefCell::new(PopupLog::default()));
    with_popups("hello world", &log, |app, _| {
        let hwnd = first_window_hwnd(app);
        let select_all = menu_id(CommandId::EditSelectAll);
        log.borrow_mut().choose = Some(select_all);
        // MK_RBUTTON at client pixel (400, 400): inside the editor at any DPI.
        send_message(hwnd, WM_RBUTTONDOWN, 0x0002, (400 << 16) | 400);
        app.pump();
        let shown = log.borrow().shown.clone();
        assert_eq!(shown.len(), 1, "one popup was shown");
        let ids: Vec<u32> = shown[0].iter().map(|(id, _)| *id).collect();
        for command in [
            CommandId::EditCut,
            CommandId::EditCopy,
            CommandId::EditPaste,
            CommandId::EditSelectAll,
        ] {
            assert!(ids.contains(&menu_id(command)), "{command:?} in {shown:?}");
        }
        assert!(
            shown[0]
                .iter()
                .all(|(id, label)| *id == 0 || !label.is_empty()),
            "every item is labeled: {shown:?}"
        );
        // The chosen item ran in the window: the whole document is selected.
        assert_eq!(selection_range(app), (0, "hello world".len()));
    });
}

#[test]
fn a_dismissed_popup_runs_nothing() {
    let log = Rc::new(RefCell::new(PopupLog::default()));
    with_popups("hello world", &log, |app, _| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_KEYDOWN, VK_END, 0);
        let before = selection_range(app);
        send_message(hwnd, WM_RBUTTONDOWN, 0x0002, (400 << 16) | 400);
        app.pump();
        assert_eq!(log.borrow().shown.len(), 1, "the popup was shown");
        assert_eq!(
            selection_range(app),
            before,
            "dismissing it changes nothing"
        );
    });
}

#[test]
fn a_menu_button_opens_its_popup_at_the_anchor_and_runs_the_choice() {
    let log = Rc::new(RefCell::new(PopupLog::default()));
    with_popups("hello", &log, |app, _| {
        let id = app.controller().windows()[0].id;
        let scale = app.controller().windows()[0].view.borrow().layout().scale();
        let request = |x: f32| {
            app.controller().push(Request::Action(
                id,
                ShellAction::Title(TitleEvent::MenuRequested {
                    menu: MenuKind::Edit,
                    anchor: Point::new(x, 40.0),
                }),
            ));
        };
        log.borrow_mut().choose = Some(menu_id(CommandId::EditSelectAll));
        request(100.0);
        app.pump();
        // Dismissed: nothing runs, the popup still opens where it was asked to.
        request(150.0);
        app.pump();
        let log = log.borrow();
        assert_eq!(log.shown.len(), 2);
        let ids: Vec<u32> = log.shown[0].iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&menu_id(CommandId::EditUndo)), "{ids:?}");
        assert!(ids.contains(&menu_id(CommandId::EditSelectAll)), "{ids:?}");
        let moved = log.positions[1].0 - log.positions[0].0;
        let expected = (50.0 * scale).round() as i32;
        assert!((moved - expected).abs() <= 1, "{moved} against {expected}");
        assert_eq!(log.positions[1].1, log.positions[0].1);
        drop(log);
        assert_eq!(selection_range(app), (0, "hello".len()));
    });
}

// ----- remote images --------------------------------------------------------------------------

const PNG_HEAD: &str = "HTTP/1.1 200 OK\r\nContent-Type: image/png";
const SECURE_PIC: &str = "https://example.com/pic.png";
const PLAIN_PIC: &str = "http://example.com/pic.png";

/// An opaque white PNG of `width` x `height` pixels.
fn png_bytes(width: u32, height: u32) -> Vec<u8> {
    let _com = ComGuard::apartment_threaded().unwrap();
    let wic = Wic::new().unwrap();
    let pixels = vec![255; width as usize * height as usize * 4];
    wic.encode_png(&DecodedImage {
        width,
        height,
        bgra: pixels,
    })
    .unwrap()
}

/// Pumps the application and paints the window until `done` holds.
fn settle(app: &UiApp, hwnd: HWND, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        app.pump();
        send_message(hwnd, WM_PAINT, 0, 0);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The height of the whole content of the editor of the first window.
fn content_height(app: &UiApp) -> f64 {
    let cell = app.controller().windows()[0].clone();
    let view = cell.view.borrow();
    view.active_tab()
        .expect("a tab is open")
        .editor
        .scroll_extent()
        .content
}

/// What the click on "Click to load" of the image `dest` in the active document sends.
fn click_to_load(app: &UiApp, dest: &str) {
    let window = app.controller().windows()[0].clone();
    let key = window
        .view
        .borrow()
        .active_tab()
        .expect("a tab is open")
        .key;
    let event = EditorEvent::Media(MediaEvent::LoadImage {
        dest: dest.to_owned(),
        range: TextRange::new(0, 1),
    });
    app.controller()
        .push(Request::Action(window.id, ShellAction::Editor(key, event)));
    app.pump();
}

/// The network of a test: `url` answers with a 240 x 180 PNG.
fn network_with_picture(url: &str) -> Arc<ScriptedHttp> {
    let http = Arc::new(ScriptedHttp::new());
    http.respond(url, PNG_HEAD, &png_bytes(240, 180));
    http
}

fn seams_with(http: &Arc<ScriptedHttp>) -> Seams {
    Seams {
        http: Some(Arc::clone(http) as Arc<dyn Http>),
        ..Seams::default()
    }
}

#[test]
fn an_https_image_loads_by_itself_and_the_document_can_opt_out() {
    let http = network_with_picture(SECURE_PIC);
    let text = format!("![a picture]({SECURE_PIC})\n");
    with_seams(&text, None, seams_with(&http), |app, _| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_PAINT, 0, 0);
        let waiting = content_height(app);
        // The worker fetches the picture, the window decodes it and lays it out at its size.
        settle(app, hwnd, "the picture", || {
            content_height(app) > waiting + 60.0
        });
        assert_eq!(http.requests(), [SECURE_PIC]);
        let loaded = content_height(app);

        // "Don't load remote images" for this document: the picture is a placeholder again...
        let doc = app.controller().windows()[0]
            .view
            .borrow()
            .active_tab()
            .expect("a tab is open")
            .entry
            .id();
        app.controller().remote_images().set_disabled(doc, true);
        settle(app, hwnd, "the placeholder", || {
            content_height(app) < loaded - 60.0
        });
        let blocked = content_height(app);
        // ... and opting back in shows the picture again without another download.
        app.controller().remote_images().set_disabled(doc, false);
        settle(app, hwnd, "the picture again", || {
            content_height(app) > blocked + 60.0
        });
        assert_eq!(http.requests(), [SECURE_PIC], "the bytes were kept");
    });
}

#[test]
fn a_plain_http_image_waits_for_the_click() {
    let http = network_with_picture(PLAIN_PIC);
    let text = format!("![a picture]({PLAIN_PIC})\n");
    with_seams(&text, None, seams_with(&http), |app, _| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_PAINT, 0, 0);
        let blocked = content_height(app);
        for _ in 0..10 {
            app.pump();
            send_message(hwnd, WM_PAINT, 0, 0);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            http.requests().is_empty(),
            "nothing is requested before the click"
        );
        assert!((content_height(app) - blocked).abs() < 1.0);

        click_to_load(app, PLAIN_PIC);
        settle(app, hwnd, "the picture after the click", || {
            content_height(app) > blocked + 60.0
        });
        assert_eq!(http.requests(), [PLAIN_PIC]);
    });
}

#[test]
fn with_the_setting_off_even_an_https_image_waits_for_the_click() {
    let http = network_with_picture(SECURE_PIC);
    let text = format!("![a picture]({SECURE_PIC})\n");
    let settings = r#"{"attachments":{"load_remote_images":false}}"#;
    with_seams(&text, Some(settings), seams_with(&http), |app, _| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_PAINT, 0, 0);
        let blocked = content_height(app);
        for _ in 0..10 {
            app.pump();
            send_message(hwnd, WM_PAINT, 0, 0);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(http.requests().is_empty(), "the setting is respected");

        click_to_load(app, SECURE_PIC);
        settle(app, hwnd, "the picture after the click", || {
            content_height(app) > blocked + 60.0
        });
        assert_eq!(http.requests(), [SECURE_PIC]);
    });
}

#[test]
fn an_image_that_cannot_be_fetched_shows_as_failed_and_is_not_asked_again() {
    // Nothing is scripted for the URL: the network is unreachable.
    let http = Arc::new(ScriptedHttp::new());
    let text = format!("![a picture]({SECURE_PIC})\n");
    with_seams(&text, None, seams_with(&http), |app, _| {
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_PAINT, 0, 0);
        settle(app, hwnd, "the failed request", || {
            http.requests().len() == 1
        });
        for _ in 0..10 {
            app.pump();
            send_message(hwnd, WM_PAINT, 0, 0);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(http.requests(), [SECURE_PIC], "one attempt");
    });
}

// ----- scrolling ------------------------------------------------------------------------------

/// A document of a few hundred screens.
fn long_document() -> String {
    (0..400)
        .map(|n| format!("Paragraph number {n} of a long document.\n\n"))
        .collect()
}

/// The area of the editor in the first window (DIPs) and the pixels per DIP.
fn editor_area(app: &UiApp) -> (ssw_ui::geometry::Rect, f32) {
    let cell = app.controller().windows()[0].clone();
    let view = cell.view.borrow();
    (view.layout().rects().editor, view.layout().scale())
}

/// A client pixel well inside the editor.
fn editor_pixel(app: &UiApp) -> (i32, i32) {
    let (area, scale) = editor_area(app);
    (
        ((area.x + area.width / 2.0) * scale) as i32,
        ((area.y + 80.0) * scale) as i32,
    )
}

fn editor_scroll(app: &UiApp) -> f32 {
    let cell = app.controller().windows()[0].clone();
    let view = cell.view.borrow();
    view.active_tab().expect("a tab is open").editor.scroll_y()
}

fn set_editor_scroll(app: &UiApp, y: f32) {
    let cell = app.controller().windows()[0].clone();
    cell.view
        .borrow_mut()
        .active_editor_mut()
        .expect("a tab is open")
        .set_scroll_y(y);
}

/// The distance one wheel notch scrolls in the first window.
fn notch(app: &UiApp) -> f64 {
    let (area, _) = editor_area(app);
    let cell = app.controller().windows()[0].clone();
    let view = cell.view.borrow();
    let line = view
        .active_tab()
        .expect("a tab is open")
        .editor
        .config()
        .line_px();
    wheel_scroll_lines().notch_distance(f64::from(line), f64::from(area.height))
}

/// `WM_MOUSEWHEEL` of `notches` (positive scrolls up) over the client pixel `at`.
fn send_wheel(hwnd: HWND, notches: i16, at: (i32, i32)) {
    let mut rect = RECT::default();
    // SAFETY: reads the rectangle of our own window into a local.
    unsafe { GetWindowRect(hwnd, &mut rect) }.unwrap();
    // The wheel position is in screen coordinates; the window has no frame around its client.
    let (x, y) = (rect.left + at.0, rect.top + at.1);
    let delta = notches.saturating_mul(120);
    send_message(
        hwnd,
        WM_MOUSEWHEEL,
        usize::from(delta as u16) << 16,
        ((y as i16 as isize) << 16) | (x as u16 as isize),
    );
}

#[test]
fn the_scroll_driver_follows_the_editor_and_animates_the_wheel() {
    with_window(&long_document(), |app, _| {
        let id = app.controller().windows()[0].id;
        let platform = app.platform(id).expect("the window has a platform");
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_PAINT, 0, 0);
        app.pump();
        assert_eq!(platform.scroll_offset(), Some(0.0), "the window is driven");
        // The system settings changed: the driver reads them again and goes on.
        send_message(hwnd, WM_SETTINGCHANGE, 0, 0);
        app.pump();

        // The editor moved by itself (the caret, a search): the driver continues from there.
        set_editor_scroll(app, 400.0);
        app.pump();
        let offset = platform.scroll_offset().unwrap();
        assert!((offset - 400.0).abs() < 0.5, "{offset}");

        // One notch down: the editor's own jump is undone and the driver animates the step.
        let at = editor_pixel(app);
        let target = f64::from(editor_scroll(app)) + notch(app);
        send_wheel(hwnd, -1, at);
        assert!(
            (editor_scroll(app) - 400.0).abs() < 0.5,
            "the jump was undone"
        );
        settle(app, hwnd, "the wheel step", || {
            (f64::from(editor_scroll(app)) - target).abs() < 0.5
        });
        let offset = platform.scroll_offset().unwrap();
        assert!((offset - target).abs() < 0.5, "{offset} against {target}");

        // DirectManipulation finishes the last fractions of the step on its own; wait until
        // the editor rests before checking that a wheel elsewhere leaves it alone.
        let mut resting = editor_scroll(app);
        let mut still_reads = 0;
        settle(app, hwnd, "the end of the animation", || {
            let now = editor_scroll(app);
            still_reads = if now == resting { still_reads + 1 } else { 0 };
            resting = now;
            still_reads >= 10
        });
        // Over the title bar the wheel is not the editor's.
        send_wheel(hwnd, -1, (2, 2));
        app.pump();
        assert_eq!(editor_scroll(app), resting);

        // Up at the top: nothing to scroll, nothing starts.
        set_editor_scroll(app, 0.0);
        app.pump();
        send_wheel(hwnd, 1, at);
        app.pump();
        assert_eq!(editor_scroll(app), 0.0);
        assert_eq!(platform.scroll_offset(), Some(0.0));
    });
}

#[test]
fn contacts_the_system_does_not_know_never_start_a_manipulation() {
    with_window(&long_document(), |app, _| {
        let id = app.controller().windows()[0].id;
        let platform = app.platform(id).expect("the window has a platform");
        let hwnd = first_window_hwnd(app);
        send_message(hwnd, WM_PAINT, 0, 0);
        app.pump();
        // Touch and touchpad contacts of pointers the system has no record of: ignored.
        send_message(hwnd, WM_POINTERDOWN, 0x7777, 0);
        send_message(hwnd, DM_POINTERHITTEST, 0x7777, 0);
        let touch_move = PointerEvent {
            kind: PointerEventKind::Move,
            button: None,
            position: Point::new(10.0, 10.0),
            pointer_type: PointerType::Touch,
            click_count: 0,
            mods: Modifiers::NONE,
        };
        assert!(!platform.is_scrolling_contact(&touch_move));
        app.pump();
        assert_eq!(platform.scroll_offset(), Some(0.0));

        // A contact scrolls the editor only where the editor is.
        let (x, y) = editor_pixel(app);
        assert!(platform.over_editor((x, y)));
        assert!(!platform.over_editor((2, 2)), "the title bar");
        assert!(!platform.over_editor((-50, y)), "outside the window");
    });
}

#[test]
fn the_scroll_driver_is_released_when_the_window_closes() {
    with_window("hello", |app, _| {
        let id = app.controller().windows()[0].id;
        let platform = app.platform(id).expect("the window has a platform");
        assert!(platform.scroll_offset().is_some());
        send_message(first_window_hwnd(app), WM_CLOSE, 0, 0);
        app.pump();
        assert_eq!(app.exit_code(), Some(0));
        assert_eq!(platform.scroll_offset(), None);
    });
}

// ----- the taskbar button -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    State(i32),
    Value(u64, u64),
}

/// A taskbar that records what it is asked.
struct RecordingApi(Rc<RefCell<Vec<Call>>>);

impl TaskbarApi for RecordingApi {
    fn set_state(&mut self, _hwnd: HWND, state: TBPFLAG) -> windows::core::Result<()> {
        self.0.borrow_mut().push(Call::State(state.0));
        Ok(())
    }

    fn set_value(&mut self, _hwnd: HWND, completed: u64, total: u64) -> windows::core::Result<()> {
        self.0.borrow_mut().push(Call::Value(completed, total));
        Ok(())
    }
}

#[derive(Debug)]
struct RecordingTaskbars(Rc<RefCell<Vec<Call>>>);

impl TaskbarFactory for RecordingTaskbars {
    fn create(&self) -> Result<Box<dyn TaskbarApi>, WinError> {
        Ok(Box::new(RecordingApi(Rc::clone(&self.0))))
    }
}

#[test]
fn the_taskbar_button_shows_its_progress_again_when_the_taskbar_recreates_it() {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let seams = Seams {
        taskbar: Some(Rc::new(RecordingTaskbars(Rc::clone(&calls)))),
        ..Seams::default()
    };
    with_seams("hello", None, seams, |app, _| {
        let id = app.controller().windows()[0].id;
        let hwnd = first_window_hwnd(app);
        // The message arrives before any progress exists: nothing to show.
        send_message(hwnd, taskbar_button_created_message(), 0, 0);
        assert!(calls.borrow().is_empty());

        app.controller().push(Request::TaskbarProgress(
            id,
            Progress::Normal {
                completed: 3,
                total: 10,
            },
        ));
        app.pump();
        assert_eq!(
            *calls.borrow(),
            [Call::State(TBPF_NORMAL.0), Call::Value(3, 10)]
        );
        calls.borrow_mut().clear();

        // Explorer restarted: the new button knows nothing, the window tells it again.
        send_message(hwnd, taskbar_button_created_message(), 0, 0);
        assert_eq!(
            *calls.borrow(),
            [Call::State(TBPF_NORMAL.0), Call::Value(3, 10)]
        );
        calls.borrow_mut().clear();

        // The window goes away: the progress does not outlive it.
        send_message(hwnd, WM_CLOSE, 0, 0);
        app.pump();
        assert_eq!(app.exit_code(), Some(0));
        assert_eq!(*calls.borrow(), [Call::State(TBPF_NOPROGRESS.0)]);
    });
}
