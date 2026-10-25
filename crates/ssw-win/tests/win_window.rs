//! Real hidden windows driven through messages sent to their own handle: creation, DWM
//! attributes, DPI handling, custom caption hit testing, input translation, timers, the message
//! loop and the close protocol. Windows are never shown, activated or given focus.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_ui::canvas::Canvas;
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::input::{Cursor, Event, PointerButton, PointerEventKind, vk};
use ssw_ui::services::{TimerId, Timers};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_win::chrome::{BUTTON_WIDTH, CAPTION_HEIGHT};
use ssw_win::dpi::{top_resize_band_px, window_dpi};
use ssw_win::theme::{SystemTheme, Win32Settings, read_system_theme};
use ssw_win::window::{
    HitRegion, ResizeEdge, WindowHandler, WindowHost, WindowInfo, WindowOptions,
    default_hit_region, quit, rethrow_pending_panic, run_loop,
};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMSBT_TABBEDWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMWINDOWATTRIBUTE, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GWL_EXSTYLE, GetWindowLongW, GetWindowRect, GetWindowTextW, HTBOTTOMRIGHT,
    HTCAPTION, HTCLIENT, HTCLOSE, HTMAXBUTTON, HTMINBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsWindow,
    IsWindowVisible, MSG, PM_REMOVE, PeekMessageW, SendMessageW, TranslateMessage, WM_CHAR,
    WM_CLOSE, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCACTIVATE, WM_NCHITTEST, WM_NCLBUTTONDOWN, WM_NCMOUSEMOVE, WM_RBUTTONDOWN,
    WM_SETTINGCHANGE, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
};

#[derive(Default)]
struct Log {
    events: Vec<Event>,
    resizes: Vec<(i32, i32)>,
    /// The DPI the handler saw with each of `resizes`.
    dpis: Vec<u32>,
    themes: Vec<SystemTheme>,
    wakes: u32,
    close_requests: u32,
    destroys: u32,
    paints: u32,
}

struct Recorder {
    log: Rc<RefCell<Log>>,
    allow_close: bool,
    /// Use a tab-strip style hit test instead of the default caption.
    custom_regions: bool,
}

impl WindowHandler for Recorder {
    fn hit_test(&self, point: Point, info: &WindowInfo) -> HitRegion {
        let size = info.size_dip();
        if self.custom_regions {
            if point.x > size.width - 40.0 && point.y > size.height - 40.0 {
                return HitRegion::Resize(ResizeEdge::BottomRight);
            }
            if point.y >= 0.0 && point.y < CAPTION_HEIGHT {
                return match default_hit_region(point, size.width) {
                    HitRegion::Caption if point.x >= 200.0 => HitRegion::Client,
                    other => other,
                };
            }
            return HitRegion::Client;
        }
        default_hit_region(point, size.width)
    }

    fn on_event(&mut self, _info: &WindowInfo, event: Event) {
        self.log.borrow_mut().events.push(event);
    }

    fn on_paint(&mut self, _info: &WindowInfo, canvas: &mut dyn Canvas) {
        self.log.borrow_mut().paints += 1;
        canvas.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), Color::BLACK);
    }

    fn on_resize(&mut self, info: &WindowInfo) {
        let mut log = self.log.borrow_mut();
        log.resizes.push(info.size_px);
        log.dpis.push(info.dpi);
    }

    fn on_theme_changed(&mut self, _info: &WindowInfo, theme: &SystemTheme) {
        self.log.borrow_mut().themes.push(*theme);
    }

    fn on_wake(&mut self, _info: &WindowInfo) {
        self.log.borrow_mut().wakes += 1;
    }

    fn on_close_requested(&mut self, _info: &WindowInfo) -> bool {
        self.log.borrow_mut().close_requests += 1;
        self.allow_close
    }

    fn on_destroy(&mut self, _info: &WindowInfo) {
        self.log.borrow_mut().destroys += 1;
    }
}

const WIDTH: i32 = 900;
const HEIGHT: i32 = 600;

fn options(dark: bool) -> WindowOptions {
    WindowOptions {
        title: "SS Writer window test".to_owned(),
        rect: Some((-32000, -32000, WIDTH, HEIGHT)),
        dark,
        no_activate: true,
    }
}

fn host_with(
    allow_close: bool,
    custom_regions: bool,
    dark: bool,
) -> (WindowHost, Rc<RefCell<Log>>) {
    let log = Rc::new(RefCell::new(Log::default()));
    let handler = Box::new(Recorder {
        log: Rc::clone(&log),
        allow_close,
        custom_regions,
    });
    let host = WindowHost::create(&options(dark), handler, Theme::light(DEFAULT_ACCENT)).unwrap();
    (host, log)
}

fn host() -> (WindowHost, Rc<RefCell<Log>>) {
    host_with(true, false, false)
}

fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    // SAFETY: a synchronous message to a window owned by this thread.
    let result = unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))) }.0;
    // A panic of the handler was parked by the window procedure: raise it in the test.
    rethrow_pending_panic();
    result
}

fn pack(x: i32, y: i32) -> isize {
    ((y as isize & 0xFFFF) << 16) | (x as isize & 0xFFFF)
}

fn screen_lparam(hwnd: HWND, x: i32, y: i32) -> isize {
    let mut point = POINT { x, y };
    // SAFETY: converts a client point of our own window.
    let _ = unsafe { ClientToScreen(hwnd, &mut point) };
    pack(point.x, point.y)
}

fn hit(hwnd: HWND, x: i32, y: i32) -> u32 {
    send(hwnd, WM_NCHITTEST, 0, screen_lparam(hwnd, x, y)) as u32
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: drains this thread's queue without blocking.
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: dispatches the message this thread just retrieved.
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        rethrow_pending_panic();
    }
}

fn pump_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        pump();
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    done()
}

fn dwm_i32(hwnd: HWND, attribute: DWMWINDOWATTRIBUTE) -> i32 {
    let mut value = -1i32;
    // SAFETY: valid out pointer of the stated size.
    unsafe {
        DwmGetWindowAttribute(hwnd, attribute, (&mut value as *mut i32).cast(), 4).unwrap();
    }
    value
}

fn px(dip: f32, scale: f32) -> i32 {
    (dip * scale).round() as i32
}

/// Asserts the standard caption layout at the window's current DPI.
fn assert_default_caption_layout(hwnd: HWND, host: &WindowHost) {
    let info = host.info();
    let scale = info.scale();
    let width_dip = info.size_px.0 as f32 / scale;
    let band = top_resize_band_px(info.dpi);
    let mid = px(CAPTION_HEIGHT / 2.0, scale);
    let button = |slot: f32| px(width_dip - BUTTON_WIDTH * slot + BUTTON_WIDTH / 2.0, scale);
    assert_eq!(
        hit(hwnd, button(1.0), mid),
        HTCLOSE,
        "close at {}",
        info.dpi
    );
    assert_eq!(
        hit(hwnd, button(2.0), mid),
        HTMAXBUTTON,
        "maximize at {}",
        info.dpi
    );
    assert_eq!(
        hit(hwnd, button(3.0), mid),
        HTMINBUTTON,
        "minimize at {}",
        info.dpi
    );
    assert_eq!(
        hit(hwnd, px(300.0, scale), mid),
        HTCAPTION,
        "caption at {}",
        info.dpi
    );
    assert_eq!(
        hit(hwnd, px(300.0, scale), px(CAPTION_HEIGHT + 20.0, scale)),
        HTCLIENT
    );
    // The invisible resize band scales with the DPI and wins over the caption.
    assert_eq!(
        hit(hwnd, px(300.0, scale), band - 1),
        HTTOP,
        "top band at {}",
        info.dpi
    );
    assert_eq!(
        hit(hwnd, px(300.0, scale), band + 1),
        HTCAPTION,
        "below the band at {}",
        info.dpi
    );
    assert_eq!(hit(hwnd, 1, 1), HTTOPLEFT);
    assert_eq!(hit(hwnd, info.size_px.0 - 2, 1), HTTOPRIGHT);
}

#[test]
fn windows_are_created_hidden_with_their_facts() {
    let (host, log) = host_with(true, false, true);
    let hwnd = host.hwnd();
    // SAFETY: plain queries on our own window.
    unsafe {
        assert!(!IsWindowVisible(hwnd).as_bool(), "never shown");
        let mut title = [0u16; 64];
        let length = GetWindowTextW(hwnd, &mut title) as usize;
        assert_eq!(
            String::from_utf16_lossy(&title[..length]),
            "SS Writer window test"
        );
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        assert_ne!(ex_style & WS_EX_NOACTIVATE.0, 0);
        assert_ne!(
            ex_style & WS_EX_NOREDIRECTIONBITMAP.0,
            0,
            "composition draws the window"
        );
        assert_eq!(GetDpiForWindow(hwnd), window_dpi(hwnd));
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).unwrap();
        assert_eq!(rect.right - rect.left, WIDTH);
        assert_eq!(rect.bottom - rect.top, HEIGHT);
    }
    let info = host.info();
    assert_eq!(info.hwnd, hwnd);
    assert_eq!(info.dpi, window_dpi(hwnd));
    assert!(info.dpi >= 96);
    assert!(
        info.size_px.0 > 700 && info.size_px.0 <= WIDTH,
        "client {:?}",
        info.size_px
    );
    assert!(info.size_px.1 > 500 && info.size_px.1 <= HEIGHT);
    assert!(!info.maximized);
    assert_eq!(log.borrow().destroys, 0);
    // The DWM attributes of the custom frame.
    assert_eq!(dwm_i32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE), 1);
    assert_eq!(
        dwm_i32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE),
        DWMSBT_TABBEDWINDOW.0
    );
    assert_eq!(
        dwm_i32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE),
        DWMWCP_ROUND.0
    );
    ssw_win::window::apply_dark_mode(hwnd, false);
    assert_eq!(dwm_i32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE), 0);
}

#[test]
fn default_options_use_the_system_placement_and_still_stay_hidden() {
    let defaults = WindowOptions::default();
    assert_eq!(
        defaults,
        WindowOptions {
            title: String::new(),
            rect: None,
            dark: false,
            no_activate: false,
        }
    );
    let log = Rc::new(RefCell::new(Log::default()));
    let handler = Box::new(Recorder {
        log,
        allow_close: true,
        custom_regions: false,
    });
    let host = WindowHost::create(&defaults, handler, Theme::dark(DEFAULT_ACCENT)).unwrap();
    // SAFETY: plain query on our own window.
    let visible = unsafe { IsWindowVisible(host.hwnd()) };
    assert!(!visible.as_bool());
    assert!(host.info().size_px.0 > 0 && host.info().size_px.1 > 0);
    assert_eq!(dwm_i32(host.hwnd(), DWMWA_USE_IMMERSIVE_DARK_MODE), 0);
}

#[test]
fn caption_hit_testing_follows_the_dpi() {
    let (host, log) = host();
    let hwnd = host.hwnd();
    assert_default_caption_layout(hwnd, &host);

    // Move the window to a 150 % monitor: the suggested rectangle is applied and every region
    // is recomputed in the new pixel space.
    let mut before = RECT::default();
    // SAFETY: valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut before) }.unwrap();
    let suggested = RECT {
        left: before.left,
        top: before.top,
        right: before.left + (WIDTH * 3) / 2,
        bottom: before.top + (HEIGHT * 3) / 2,
    };
    let resizes_before = log.borrow().resizes.len();
    send(
        hwnd,
        WM_DPICHANGED,
        (144 << 16) | 144,
        (&suggested as *const RECT) as isize,
    );
    let info = host.info();
    assert_eq!(info.dpi, 144);
    assert_eq!(info.scale(), 1.5);
    assert!(
        log.borrow().resizes.len() > resizes_before,
        "the new size reached the handler"
    );
    assert_eq!(log.borrow().resizes.last().copied(), Some(info.size_px));
    assert_eq!(log.borrow().dpis.last().copied(), Some(144));
    let mut after = RECT::default();
    // SAFETY: valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut after) }.unwrap();
    assert_eq!(after.right - after.left, (WIDTH * 3) / 2);
    assert_default_caption_layout(hwnd, &host);
}

#[test]
fn handlers_can_reclassify_regions() {
    let (host, _log) = host_with(true, true, false);
    let hwnd = host.hwnd();
    let info = host.info();
    let scale = info.scale();
    let (width, height) = info.size_px;
    // Only the tab strip is caption; the rest of the top strip belongs to the client.
    assert_eq!(hit(hwnd, px(100.0, scale), px(16.0, scale)), HTCAPTION);
    assert_eq!(hit(hwnd, px(400.0, scale), px(16.0, scale)), HTCLIENT);
    // The caption buttons keep their regions.
    let close_x = width - px(BUTTON_WIDTH / 2.0, scale);
    assert_eq!(hit(hwnd, close_x, px(16.0, scale)), HTCLOSE);
    // A custom resize grip inside the client area.
    assert_eq!(hit(hwnd, width - 20, height - 20), HTBOTTOMRIGHT);
    assert_eq!(hit(hwnd, width - 200, height - 200), HTCLIENT);
    // The top resize band still wins over handler answers.
    assert_eq!(hit(hwnd, px(100.0, scale), 1), HTTOP);
}

#[test]
fn caption_button_presses_are_swallowed_until_released() {
    let (host, log) = host_with(false, false, false);
    let hwnd = host.hwnd();
    // Hover tracking and presses on the custom buttons never reach DefWindowProc's modal
    // caption loops, and a refused close keeps the window alive.
    send(hwnd, WM_NCMOUSEMOVE, HTCLOSE as usize, 0);
    assert_eq!(send(hwnd, WM_NCLBUTTONDOWN, HTCLOSE as usize, 0), 0);
    send(
        hwnd,
        windows::Win32::UI::WindowsAndMessaging::WM_NCLBUTTONUP,
        HTCLOSE as usize,
        0,
    );
    assert!(pump_until(Duration::from_secs(2), || log
        .borrow()
        .close_requests
        == 1));
    // SAFETY: plain query.
    let alive = unsafe { IsWindow(Some(hwnd)) };
    assert!(alive.as_bool(), "the handler refused to close");
    assert_eq!(log.borrow().destroys, 0);
}

#[test]
fn pointer_text_and_key_messages_become_events_in_dips() {
    let (host, log) = host();
    let hwnd = host.hwnd();
    // Work at 150 %.
    let mut rect = RECT::default();
    // SAFETY: valid out pointer.
    unsafe { GetWindowRect(hwnd, &mut rect) }.unwrap();
    let (resizes_before, dpis_before) = (log.borrow().resizes.len(), log.borrow().dpis.len());
    send(
        hwnd,
        WM_DPICHANGED,
        (144 << 16) | 144,
        (&rect as *const RECT) as isize,
    );
    assert_eq!(host.info().scale(), 1.5);
    // The suggested rectangle is the window's own, so no WM_SIZE follows; the handler still
    // learns the new scale, once.
    assert_eq!(log.borrow().resizes.len(), resizes_before + 1);
    assert_eq!(log.borrow().dpis[dpis_before..], [144]);
    log.borrow_mut().events.clear();

    send(hwnd, WM_MOUSEMOVE, 0, pack(150, 90));
    send(hwnd, WM_LBUTTONDOWN, 1, pack(150, 90));
    send(hwnd, WM_LBUTTONUP, 0, pack(150, 90));
    send(hwnd, WM_LBUTTONDOWN, 1, pack(150, 90));
    send(hwnd, WM_RBUTTONDOWN, 2, pack(30, 45));
    send(hwnd, WM_MOUSEWHEEL, 120 << 16, screen_lparam(hwnd, 150, 90));
    send(hwnd, WM_KEYDOWN, usize::from(vk::RETURN), 0x001C_0001);
    send(hwnd, WM_CHAR, 'ğ' as usize, 1);
    send(hwnd, WM_CHAR, 0x0D, 1);
    send(hwnd, WM_CHAR, 0xD83D, 1);
    send(hwnd, WM_CHAR, 0xDE00, 1);
    // Focus and pointer messages are not sent to a real window: their default processing
    // could move keyboard focus. The unit tests of `window` feed them to the state directly.

    let events = log.borrow().events.clone();
    let pointers: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Pointer(pointer) => Some((
                pointer.kind,
                pointer.button,
                pointer.position,
                pointer.click_count,
            )),
            _ => None,
        })
        .collect();
    let at = Point::new(100.0, 60.0);
    assert_eq!(
        pointers,
        vec![
            (PointerEventKind::Move, None, at, 0),
            (PointerEventKind::Down, Some(PointerButton::Left), at, 1),
            (PointerEventKind::Up, Some(PointerButton::Left), at, 0),
            (PointerEventKind::Down, Some(PointerButton::Left), at, 2),
            (
                PointerEventKind::Down,
                Some(PointerButton::Right),
                Point::new(20.0, 30.0),
                1
            ),
        ]
    );
    let wheel = events
        .iter()
        .find_map(|event| match event {
            Event::Wheel(wheel) => Some(*wheel),
            _ => None,
        })
        .unwrap();
    assert_eq!(wheel.delta, Point::new(0.0, 1.0));
    assert!(!wheel.precise);
    assert_eq!(wheel.position, at);
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            Event::Text(text) => Some(text.0.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["ğ", "😀"], "Enter is a key, surrogate halves join");
    let key = events
        .iter()
        .find_map(|event| match event {
            Event::Key(key) => Some(*key),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (key.vk, key.scan, key.is_down, key.repeat),
        (vk::RETURN, 0x1C, true, false)
    );
    // Five pointer events, one wheel, one key and two text events.
    assert_eq!(events.len(), 5 + 1 + 1 + 2, "nothing else was emitted");
}

#[test]
fn non_client_activation_dims_the_chrome() {
    let (host, _log) = host();
    let hwnd = host.hwnd();
    // Only the non-client activation (frame state, never focus) is sent to the real window:
    // `WM_ACTIVATE` and `WM_SETFOCUS` would go through default processing and could move
    // keyboard focus, so the unit tests of `window` feed those to the state directly.
    send(hwnd, WM_NCACTIVATE, 1, 0);
    assert!(host.info().active);
    send(hwnd, WM_NCACTIVATE, 0, 0);
    assert!(
        !host.info().active,
        "the chrome dims when the window is inactive"
    );
}

#[test]
fn theme_changes_update_the_dwm_dark_mode_and_notify_the_handler() {
    let (host, log) = host();
    let hwnd = host.hwnd();
    let unrelated: Vec<u16> = "intl\0".encode_utf16().collect();
    send(hwnd, WM_SETTINGCHANGE, 0, unrelated.as_ptr() as isize);
    assert!(log.borrow().themes.is_empty());
    let setting: Vec<u16> = "ImmersiveColorSet\0".encode_utf16().collect();
    send(hwnd, WM_SETTINGCHANGE, 0, setting.as_ptr() as isize);
    let expected = read_system_theme(&Win32Settings);
    let themes = log.borrow().themes.clone();
    assert_eq!(themes.len(), 1);
    assert_eq!(themes[0].dark, expected.dark);
    assert_eq!(themes[0].theme, expected.theme);
    assert_eq!(
        dwm_i32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE),
        i32::from(expected.dark)
    );
}

#[test]
fn timers_deliver_events_and_frames_arrive_on_request() {
    let (host, log) = host();
    let mut timers = host.timers();
    let (once, repeating) = (TimerId::allocate(), TimerId::allocate());
    let count = |id: TimerId| {
        log.borrow()
            .events
            .iter()
            .filter(|event| **event == Event::Timer(id))
            .count()
    };
    timers.set(once, Duration::from_millis(20), false);
    timers.set(repeating, Duration::from_millis(20), true);
    assert!(pump_until(Duration::from_secs(3), || count(once) == 1
        && count(repeating) >= 2));
    // The one-shot timer fired once and is gone; the repeating one stops when killed.
    timers.kill(repeating);
    timers.kill(once);
    pump();
    let settled = count(repeating);
    assert!(!pump_until(Duration::from_millis(150), || count(repeating) > settled));
    assert_eq!(count(once), 1);

    // Animation frames: nothing is delivered until one is requested, then exactly one.
    assert!(!host.deliver_animation_frame());
    timers.request_animation_frame();
    assert!(host.deliver_animation_frame());
    assert!(!host.deliver_animation_frame());
    let frames: Vec<Duration> = log
        .borrow()
        .events
        .iter()
        .filter_map(|event| match event {
            Event::AnimationFrame(time) => Some(*time),
            _ => None,
        })
        .collect();
    assert_eq!(frames.len(), 1);
    assert!(frames[0] > Duration::ZERO);
}

#[test]
fn the_message_loop_runs_until_quit_and_workers_wake_it() {
    let (host, log) = host();
    let wake = host.wake_handle();
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        wake.wake()
    });
    let started = Instant::now();
    let idles = Rc::new(RefCell::new(0u32));
    let code = run_loop({
        let log = Rc::clone(&log);
        let idles = Rc::clone(&idles);
        move || {
            *idles.borrow_mut() += 1;
            if log.borrow().wakes >= 1 {
                quit(42);
            } else if started.elapsed() > Duration::from_secs(10) {
                quit(-1);
            }
            Some(Duration::from_millis(500))
        }
    });
    assert_eq!(code, 42);
    assert!(worker.join().unwrap(), "the wake reached a live window");
    assert_eq!(log.borrow().wakes, 1);
    assert!(
        *idles.borrow() >= 2,
        "the loop idled before and after the wake"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn closing_negotiates_with_the_handler_and_destroys_the_window() {
    let (refusing, refusing_log) = host_with(false, false, false);
    send(refusing.hwnd(), WM_CLOSE, 0, 0);
    assert_eq!(refusing_log.borrow().close_requests, 1);
    assert_eq!(refusing_log.borrow().destroys, 0);
    // SAFETY: plain query.
    let refusing_alive = unsafe { IsWindow(Some(refusing.hwnd())) };
    assert!(refusing_alive.as_bool());

    let (accepting, accepting_log) = host_with(true, false, false);
    let hwnd = accepting.hwnd();
    let wake = accepting.wake_handle();
    send(hwnd, WM_CLOSE, 0, 0);
    assert_eq!(accepting_log.borrow().destroys, 1);
    // SAFETY: plain query.
    let accepting_alive = unsafe { IsWindow(Some(hwnd)) };
    assert!(!accepting_alive.as_bool());
    assert!(!wake.wake(), "waking a destroyed window reports failure");

    // `destroy` skips the negotiation.
    refusing.destroy();
    assert_eq!(refusing_log.borrow().destroys, 1);
    assert_eq!(refusing_log.borrow().close_requests, 1);
}

#[test]
fn cursor_and_theme_setters_work_before_graphics_exist() {
    let (host, log) = host();
    host.set_cursor(Cursor::Hand);
    host.set_theme(Theme::dark(DEFAULT_ACCENT));
    host.invalidate();
    assert_eq!(host.graphics_generation(), None);
    assert_eq!(host.with_assets(|assets| assets.images.len()), None);
    // Invalidation only schedules a repaint; without graphics nothing is painted.
    pump();
    assert_eq!(log.borrow().paints, 0);
}
