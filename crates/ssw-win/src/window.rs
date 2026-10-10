//! The native window: class registration, custom title bar, DirectComposition rendering,
//! message translation into [`ssw_ui::input::Event`]s and the message loop.
//!
//! The window keeps `WS_OVERLAPPEDWINDOW` (so Aero Snap, the shadow and the min/max animations
//! keep working) but gives the whole non-client top strip back to the client area in
//! `WM_NCCALCSIZE`. The app draws the caption buttons ([`crate::chrome`]) and answers hit tests
//! through [`WindowHandler::hit_test`].

use std::any::Any;
use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ssw_ui::canvas::Canvas;
use ssw_ui::geometry::{Point, Size};
use ssw_ui::input::{
    Cursor, Event, FocusEvent, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind,
    PointerType, TextInput, WheelEvent, vk,
};
use ssw_ui::theme::Theme;
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DWMSBT_TABBEDWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmExtendFrameIntoClientArea,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{InvalidateRect, ScreenToClient, ValidateRect};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
    TRACKMOUSEEVENT_FLAGS, TrackMouseEvent,
};
use windows::Win32::UI::Input::Pointer::{GetPointerInfo, GetPointerType, POINTER_INFO};
use windows::Win32::UI::Shell::{ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_DBLCLKS, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, HTBOTTOM, HTBOTTOMLEFT,
    HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTCLOSE, HTLEFT, HTMAXBUTTON, HTMINBUTTON, HTRIGHT, HTTOP,
    HTTOPLEFT, HTTOPRIGHT, HWND_MESSAGE, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM,
    IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, IsZoomed,
    LoadCursorW, MINMAXINFO, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx,
    NCCALCSIZE_PARAMS, PM_REMOVE, PeekMessageW, PostMessageW, PostQuitMessage, QS_ALLINPUT,
    RegisterClassExW, SIZE_MINIMIZED, SM_CXPADDEDBORDER, SM_CYSIZEFRAME, SPI_GETWHEELSCROLLLINES,
    SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    TranslateMessage, WA_INACTIVE, WINDOW_EX_STYLE, WM_ACTIVATE, WM_APP, WM_CHAR, WM_CLOSE,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST,
    WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_PAINT, WM_POINTERDOWN,
    WM_POINTERLEAVE, WM_POINTERUP, WM_POINTERUPDATE, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SETCURSOR, WM_SETFOCUS, WM_SETTINGCHANGE, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::canvas::{CanvasAssets, D2dCanvas, TargetResources};
use crate::chrome::{self, CaptionButton, ChromeState, paint_caption_buttons};
use crate::com::{OsResultExt, WinError, to_wide};
use crate::dpi::{
    DEFAULT_DPI, dip_to_px, dpi_from_wparam, min_track_size_px, point_px_to_dip, scale_for_dpi,
    size_px_to_dip, top_resize_band_px,
};
use crate::gfx::{GfxDevices, WindowSurface};
use crate::services::timers::TimerTable;
use crate::theme::{SystemTheme, Win32Settings, is_theme_change, read_system_theme};

/// Posted by [`WakeHandle::wake`]; delivered to [`WindowHandler::on_wake`].
pub const WM_WAKE: u32 = WM_APP + 1;
/// Process exit code when the OS is older than Windows 11 22H2.
pub const EXIT_UNSUPPORTED_OS: i32 = 3;
/// Oldest supported Windows build (Windows 11 22H2, which has `DWMWA_SYSTEMBACKDROP_TYPE`).
pub const MIN_OS_BUILD: u32 = 22621;

const CLASS_NAME: PCWSTR = w!("SSWriterWindow");
const MESSAGE_CLASS_NAME: PCWSTR = w!("SSWriterMessageWindow");
/// `SIZE_MAXIMIZED` etc. are not needed; only minimized windows skip rendering.
const WHEEL_DELTA: f32 = 120.0;

/// A Windows version triple.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsVersion {
    pub major: u32,
    pub build: u32,
}

/// The running Windows version: major 10 with the build from the registry.
#[must_use]
pub fn current_os_version() -> OsVersion {
    let mut buffer = [0u16; 16];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: valid key path and a writable buffer of `size` bytes; read-only access.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            Some(&mut size),
        )
    };
    let build = if status.is_ok() {
        crate::com::from_wide(&buffer).trim().parse().unwrap_or(0)
    } else {
        0
    };
    OsVersion { major: 10, build }
}

/// Checks that the OS is Windows 11 22H2 or newer. When it is not, `notify` is called (the app
/// shows its localized message box there) and the exit code to terminate with is returned.
pub fn check_os_version(version: OsVersion, notify: impl FnOnce()) -> Option<i32> {
    if version.major > 10 || (version.major == 10 && version.build >= MIN_OS_BUILD) {
        None
    } else {
        notify();
        Some(EXIT_UNSUPPORTED_OS)
    }
}

/// Shows a blocking message box with an OK button.
pub fn show_message_box(owner: Option<HWND>, title: &str, text: &str) {
    show_message_box_with(owner, title, text, |owner, title, text| {
        // SAFETY: both buffers are NUL-terminated and outlive the call.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::MessageBoxW(
                owner,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::MB_OK
                    | windows::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    });
}

/// Prepares the NUL-terminated UTF-16 `title` and `text` of a message box and hands them,
/// with `owner`, to `present`. [`show_message_box`] passes the `MessageBoxW` call; tests pass
/// a recorder, because a real message box would block on the user's screen.
pub fn show_message_box_with(
    owner: Option<HWND>,
    title: &str,
    text: &str,
    present: impl FnOnce(Option<HWND>, &[u16], &[u16]),
) {
    present(owner, &to_wide(title), &to_wide(text));
}

/// Where a point of the client area belongs, as answered by [`WindowHandler::hit_test`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitRegion {
    /// Ordinary client area.
    Client,
    /// Draggable caption area (drag, double-click maximizes, system menu).
    Caption,
    Minimize,
    Maximize,
    Close,
    /// A resize edge or corner.
    Resize(ResizeEdge),
}

/// A window edge or corner that can be dragged to resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeEdge {
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl HitRegion {
    /// The `WM_NCHITTEST` result for this region.
    #[must_use]
    pub fn hit_code(self) -> u32 {
        match self {
            HitRegion::Client => HTCLIENT,
            HitRegion::Caption => HTCAPTION,
            HitRegion::Minimize => HTMINBUTTON,
            HitRegion::Maximize => HTMAXBUTTON,
            HitRegion::Close => HTCLOSE,
            HitRegion::Resize(ResizeEdge::Top) => HTTOP,
            HitRegion::Resize(ResizeEdge::Bottom) => HTBOTTOM,
            HitRegion::Resize(ResizeEdge::Left) => HTLEFT,
            HitRegion::Resize(ResizeEdge::Right) => HTRIGHT,
            HitRegion::Resize(ResizeEdge::TopLeft) => HTTOPLEFT,
            HitRegion::Resize(ResizeEdge::TopRight) => HTTOPRIGHT,
            HitRegion::Resize(ResizeEdge::BottomLeft) => HTBOTTOMLEFT,
            HitRegion::Resize(ResizeEdge::BottomRight) => HTBOTTOMRIGHT,
        }
    }
}

/// The caption button a `WM_NC*` hit-test code stands for.
fn button_for_hit(hit: u32) -> Option<CaptionButton> {
    match hit {
        HTMINBUTTON => Some(CaptionButton::Minimize),
        HTMAXBUTTON => Some(CaptionButton::Maximize),
        HTCLOSE => Some(CaptionButton::Close),
        _ => None,
    }
}

/// The top resize edge or corner for a point in the top band of a non-maximized window.
#[must_use]
pub fn top_resize_region(point: Point, width: f32, band: f32) -> Option<HitRegion> {
    if point.y >= band {
        return None;
    }
    let edge = if point.x < band {
        ResizeEdge::TopLeft
    } else if point.x >= width - band {
        ResizeEdge::TopRight
    } else {
        ResizeEdge::Top
    };
    Some(HitRegion::Resize(edge))
}

/// The default hit test: the top [`chrome::CAPTION_HEIGHT`] strip is caption, with the three
/// caption buttons at its right end.
#[must_use]
pub fn default_hit_region(point: Point, width: f32) -> HitRegion {
    if let Some(button) = chrome::button_at(width, point) {
        return match button {
            CaptionButton::Minimize => HitRegion::Minimize,
            CaptionButton::Maximize => HitRegion::Maximize,
            CaptionButton::Close => HitRegion::Close,
        };
    }
    if point.y >= 0.0 && point.y < chrome::CAPTION_HEIGHT {
        HitRegion::Caption
    } else {
        HitRegion::Client
    }
}

/// Facts about the window passed to every handler callback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowInfo {
    pub hwnd: HWND,
    pub dpi: u32,
    /// Client size in pixels.
    pub size_px: (i32, i32),
    pub maximized: bool,
    pub active: bool,
}

impl WindowInfo {
    /// `dpi / 96`.
    #[must_use]
    pub fn scale(&self) -> f32 {
        scale_for_dpi(self.dpi)
    }

    /// Client size in DIPs.
    #[must_use]
    pub fn size_dip(&self) -> Size {
        size_px_to_dip(self.size_px.0, self.size_px.1, self.scale())
    }
}

/// The application side of a window.
pub trait WindowHandler {
    /// Classifies a client point (DIPs) for `WM_NCHITTEST`.
    fn hit_test(&self, point: Point, info: &WindowInfo) -> HitRegion {
        default_hit_region(point, info.size_dip().width)
    }
    /// Receives translated input.
    fn on_event(&mut self, info: &WindowInfo, event: Event);
    /// Paints the window content; the caption buttons are painted on top by the host.
    fn on_paint(&mut self, info: &WindowInfo, canvas: &mut dyn Canvas);
    /// The client size or DPI changed.
    fn on_resize(&mut self, _info: &WindowInfo) {}
    /// The window was activated or deactivated.
    fn on_activate(&mut self, _info: &WindowInfo, _active: bool) {}
    /// The system theme changed (`WM_SETTINGCHANGE ImmersiveColorSet`).
    fn on_theme_changed(&mut self, _info: &WindowInfo, _theme: &SystemTheme) {}
    /// A worker called [`WakeHandle::wake`].
    fn on_wake(&mut self, _info: &WindowInfo) {}
    /// The user asked to close the window; return false to keep it open.
    fn on_close_requested(&mut self, _info: &WindowInfo) -> bool {
        true
    }
    /// The window is being destroyed.
    fn on_destroy(&mut self, _info: &WindowInfo) {}
    /// Sees every message before the window's own translation. `Some(result)` answers the
    /// message (UIA's `WM_GETOBJECT`); `None` lets it continue (raw `WM_POINTER*` messages for
    /// DirectManipulation are observed this way).
    fn on_message(
        &mut self,
        _info: &WindowInfo,
        _msg: u32,
        _wparam: WPARAM,
        _lparam: LPARAM,
    ) -> Option<LRESULT> {
        None
    }
}

/// Creation parameters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WindowOptions {
    pub title: String,
    /// Position and size in pixels; `None` uses the system default placement.
    pub rect: Option<(i32, i32, i32, i32)>,
    pub dark: bool,
    /// Never activate or take focus (tests, background windows).
    pub no_activate: bool,
}

/// Sets `DWMWA_USE_IMMERSIVE_DARK_MODE`.
pub fn apply_dark_mode(hwnd: HWND, dark: bool) {
    set_dwm_i32(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE.0, i32::from(dark));
}

fn set_dwm_i32(hwnd: HWND, attribute: i32, value: i32) {
    // SAFETY: the pointer references a live i32 of the stated size; failure (an older OS
    // that lacks the attribute) is deliberately ignored: the window then has no backdrop.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(attribute),
            (&value as *const i32).cast(),
            size_of::<i32>() as u32,
        );
    }
}

/// Posts [`WM_WAKE`] to a window from any thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeHandle {
    hwnd: isize,
}

impl WakeHandle {
    /// A handle that wakes the thread of `hwnd` (any window or message-only window of it).
    #[must_use]
    pub fn new(hwnd: HWND) -> Self {
        Self {
            hwnd: hwnd.0 as isize,
        }
    }

    /// Wakes the UI thread; returns false when the window no longer exists.
    pub fn wake(&self) -> bool {
        // SAFETY: posting to a stale handle merely fails.
        unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                WM_WAKE,
                WPARAM(0),
                LPARAM(0),
            )
        }
        .is_ok()
    }
}

/// Tracks click counts: presses of the same button near the previous one within the
/// double-click time extend the count up to three.
#[derive(Debug, Clone, Copy)]
pub struct ClickTracker {
    last: Option<(Duration, Point, PointerButton, u8)>,
    interval: Duration,
    slop: f32,
}

impl ClickTracker {
    #[must_use]
    pub fn new(interval: Duration, slop: f32) -> Self {
        Self {
            last: None,
            interval,
            slop,
        }
    }

    /// The click count for a press at `time` and `position`.
    pub fn press(&mut self, time: Duration, position: Point, button: PointerButton) -> u8 {
        let count = match self.last {
            Some((previous, at, previous_button, count))
                if previous_button == button
                    && time.saturating_sub(previous) <= self.interval
                    && (position.x - at.x).abs() <= self.slop
                    && (position.y - at.y).abs() <= self.slop =>
            {
                count % 3 + 1
            }
            _ => 1,
        };
        self.last = Some((time, position, button, count));
        count
    }
}

/// Modifier keys from a key-state query (`true` while the virtual key is down).
#[must_use]
pub fn modifiers_from(down: impl Fn(u16) -> bool) -> Modifiers {
    let mut mods = Modifiers::NONE;
    if down(vk::SHIFT) {
        mods |= Modifiers::SHIFT;
    }
    if down(vk::CONTROL) {
        mods |= Modifiers::CTRL;
    }
    if down(vk::MENU) {
        mods |= Modifiers::ALT;
    }
    if down(0x5B) || down(0x5C) {
        mods |= Modifiers::WIN;
    }
    mods
}

fn current_modifiers() -> Modifiers {
    modifiers_from(|key| {
        // SAFETY: pure state query.
        unsafe { GetKeyState(i32::from(key)) < 0 }
    })
}

/// A key message as a [`KeyEvent`]: bit 30 of `lparam` marks a repeat, bits 16..24 are the scan
/// code and bit 24 the extended-key flag (reported as `0xE000 | scan`).
#[must_use]
pub fn key_event(vk: u16, lparam: isize, is_down: bool, mods: Modifiers) -> KeyEvent {
    let scan = ((lparam >> 16) & 0xFF) as u16;
    let extended = (lparam >> 24) & 1 == 1;
    KeyEvent {
        vk,
        scan: if extended { 0xE000 | scan } else { scan },
        mods,
        repeat: is_down && (lparam >> 30) & 1 == 1,
        is_down,
    }
}

/// Turns `WM_CHAR` code units into text: control characters are dropped (shortcuts and keys
/// like Enter arrive as key events) and surrogate halves are combined across messages.
pub fn text_from_char(unit: u16, pending_high: &mut Option<u16>) -> Option<String> {
    if (0xD800..0xDC00).contains(&unit) {
        *pending_high = Some(unit);
        return None;
    }
    let decoded = if (0xDC00..0xE000).contains(&unit) {
        let high = pending_high.take()?;
        char::decode_utf16([high, unit]).next()?.ok()
    } else {
        *pending_high = None;
        char::from_u32(u32::from(unit))
    }?;
    if decoded.is_control() {
        None
    } else {
        Some(decoded.to_string())
    }
}

/// A wheel message as a [`WheelEvent`]; `delta` is the signed `WHEEL_DELTA` multiple
/// (fractions of a notch from high-resolution wheels are kept).
#[must_use]
pub fn wheel_event(delta: i16, horizontal: bool, position: Point, mods: Modifiers) -> WheelEvent {
    let notches = f32::from(delta) / WHEEL_DELTA;
    WheelEvent {
        delta: if horizontal {
            Point::new(notches, 0.0)
        } else {
            Point::new(0.0, notches)
        },
        precise: false,
        position,
        mods,
    }
}

fn cursor_resource(cursor: Cursor) -> PCWSTR {
    match cursor {
        Cursor::Arrow => IDC_ARROW,
        Cursor::IBeam => IDC_IBEAM,
        Cursor::Hand => IDC_HAND,
        Cursor::Cross => IDC_CROSS,
        Cursor::SizeWE => IDC_SIZEWE,
        Cursor::SizeNS => IDC_SIZENS,
        Cursor::SizeNWSE => IDC_SIZENWSE,
        Cursor::SizeNESW => IDC_SIZENESW,
        Cursor::SizeAll => IDC_SIZEALL,
        Cursor::NotAllowed => IDC_NO,
        Cursor::Wait => IDC_WAIT,
        Cursor::Help => IDC_HELP,
    }
}

fn instance() -> HINSTANCE {
    // SAFETY: querying the handle of the current module.
    unsafe { GetModuleHandleW(None) }
        .map(|module| HINSTANCE(module.0))
        .unwrap_or_default()
}

fn register_classes() -> Result<(), WinError> {
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.load(Ordering::Acquire) {
        return Ok(());
    }
    let register = |name: PCWSTR,
                    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
                    style|
     -> Result<(), WinError> {
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            // No CS_HREDRAW / CS_VREDRAW: they force a full invalidate on every resize.
            style,
            lpfnWndProc: Some(proc),
            hInstance: instance(),
            // SAFETY: loading a stock cursor.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            lpszClassName: name,
            ..Default::default()
        };
        // SAFETY: the class structure is fully initialized and outlives the call.
        let atom = unsafe { RegisterClassExW(&class) };
        // SAFETY: reads the calling thread's last error right after the failed call.
        if atom == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
            return Err(WinError::from_os(
                "RegisterClassExW",
                windows::core::Error::from_thread(),
            ));
        }
        Ok(())
    };
    register(CLASS_NAME, wndproc, CS_DBLCLKS)?;
    register(MESSAGE_CLASS_NAME, message_proc, Default::default())?;
    REGISTERED.store(true, Ordering::Release);
    Ok(())
}

thread_local! {
    /// The first panic a window procedure caught on this thread, kept until
    /// [`rethrow_pending_panic`] raises it where unwinding is allowed.
    static PENDING_PANIC: RefCell<Option<Box<dyn Any + Send>>> = const { RefCell::new(None) };
}

/// Keeps the first panic `payload` of a window procedure; later ones are dropped.
fn stash_panic(payload: Box<dyn Any + Send>) {
    PENDING_PANIC.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(payload);
        }
    });
}

/// Raises the panic a window procedure of this thread caught, if there is one.
///
/// A panic must not unwind through the operating system's callback frames, so the window
/// procedure catches it, answers the message with default processing and parks the panic.
/// [`run_loop`] and `WindowHost` drop call this after the messages they dispatch; code that
/// sends messages itself (tests) calls it after `SendMessageW` returns.
pub fn rethrow_pending_panic() {
    let payload = PENDING_PANIC.with(|slot| slot.borrow_mut().take());
    if let Some(payload) = payload {
        resume_unwind(payload);
    }
}

/// Sends a message to one of this thread's windows and returns the window procedure's result.
/// A panic the procedure caught is raised here (see [`rethrow_pending_panic`]). Used by the
/// close negotiation and by tests that drive their own window.
pub fn send_message(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
    // SAFETY: a plain send to a window of the calling thread; the arguments are values the
    // receiving procedure interprets itself (callers pass no pointers).
    let result = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            msg,
            Some(WPARAM(wparam)),
            Some(LPARAM(lparam)),
        )
    };
    rethrow_pending_panic();
    result.0
}

unsafe extern "system" fn message_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: forwards to the default procedure with the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A hidden message-only window: an owner for the clipboard and timers.
#[derive(Debug)]
pub struct MessageWindow {
    hwnd: HWND,
}

impl MessageWindow {
    /// Creates the window.
    ///
    /// # Errors
    /// Fails when the class or window cannot be created.
    pub fn new() -> Result<Self, WinError> {
        register_classes()?;
        // SAFETY: creates a message-only window with a registered class.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                MESSAGE_CLASS_NAME,
                w!(""),
                Default::default(),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance()),
                None,
            )
        }
        .ctx("create message window")?;
        Ok(Self { hwnd })
    }

    #[must_use]
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window created in `new` on its owning thread.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct Graphics {
    devices: GfxDevices,
    surface: WindowSurface,
    resources: TargetResources,
    assets: CanvasAssets,
}

struct WindowState {
    hwnd: HWND,
    handler: Box<dyn WindowHandler>,
    dpi: u32,
    size_px: (i32, i32),
    chrome: ChromeState,
    theme: Theme,
    graphics: Option<Graphics>,
    pending_high_surrogate: Option<u16>,
    clicks: ClickTracker,
    started: Instant,
    cursor: Cursor,
    tracking_client: bool,
    tracking_nonclient: bool,
    timers: Rc<RefCell<TimerTable>>,
    painting: bool,
}

/// Work to do after the state borrow of a message is released (window functions that send
/// messages back into the procedure).
enum After {
    None,
    SetRect(RECT),
    Close,
    Show(i32),
}

impl WindowState {
    fn info(&self) -> WindowInfo {
        WindowInfo {
            hwnd: self.hwnd,
            dpi: self.dpi,
            size_px: self.size_px,
            // SAFETY: plain state query on our own window.
            maximized: unsafe { IsZoomed(self.hwnd) }.as_bool(),
            active: self.chrome.active,
        }
    }

    fn scale(&self) -> f32 {
        scale_for_dpi(self.dpi)
    }

    fn client_point(&self, screen: POINT) -> Point {
        let mut point = screen;
        // SAFETY: converts using our own window handle.
        let _ = unsafe { ScreenToClient(self.hwnd, &mut point) };
        point_px_to_dip(point.x, point.y, self.scale())
    }

    fn dispatch(&mut self, event: Event) {
        let info = self.info();
        self.handler.on_event(&info, event);
    }

    fn invalidate(&self) {
        // SAFETY: invalidates our own window; the repaint arrives as WM_PAINT.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn set_hover(&mut self, hover: Option<CaptionButton>) {
        if self.chrome.hover != hover {
            self.chrome.hover = hover;
            self.invalidate();
        }
    }

    fn track(&mut self, nonclient: bool) {
        let flag = if nonclient {
            &mut self.tracking_nonclient
        } else {
            &mut self.tracking_client
        };
        if *flag {
            return;
        }
        *flag = true;
        let mut request = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE
                | if nonclient {
                    TME_NONCLIENT
                } else {
                    TRACKMOUSEEVENT_FLAGS(0)
                },
            hwndTrack: self.hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: valid request structure for our own window.
        unsafe {
            let _ = TrackMouseEvent(&mut request);
        }
    }

    fn pointer(
        &mut self,
        kind: PointerEventKind,
        button: Option<PointerButton>,
        position: Point,
        pointer_type: PointerType,
    ) {
        let click_count = if kind == PointerEventKind::Down {
            let time = self.started.elapsed();
            button.map_or(0, |button| self.clicks.press(time, position, button))
        } else {
            0
        };
        self.dispatch(Event::Pointer(PointerEvent {
            kind,
            button,
            position,
            pointer_type,
            click_count,
            mods: current_modifiers(),
        }));
    }

    /// Resizes the swap chain and repaints immediately (flicker-free live resize).
    fn resize(&mut self, width: i32, height: i32) {
        self.size_px = (width, height);
        if let Some(graphics) = self.graphics.as_mut() {
            graphics.surface.set_dpi(self.dpi as f32);
            let resized = graphics
                .surface
                .resize(width.max(1) as u32, height.max(1) as u32);
            if let Err(error) = resized
                && error.is_device_lost()
            {
                self.recover_graphics();
            }
        }
        let info = self.info();
        self.handler.on_resize(&info);
        self.paint();
    }

    fn paint(&mut self) {
        if self.painting || self.size_px.0 <= 0 || self.size_px.1 <= 0 {
            return;
        }
        self.painting = true;
        let outcome = self.paint_once();
        if matches!(&outcome, Err(error) if error.is_device_lost()) {
            self.recover_graphics();
            let _ = self.paint_once();
        }
        self.painting = false;
    }

    fn paint_once(&mut self) -> Result<(), WinError> {
        let info = self.info();
        let width = info.size_dip().width;
        let chrome_state = ChromeState {
            maximized: info.maximized,
            ..self.chrome
        };
        let tokens = self.theme.tokens;
        let Some(graphics) = self.graphics.as_mut() else {
            return Ok(());
        };
        let Graphics {
            surface,
            resources,
            assets,
            ..
        } = graphics;
        let handler = &mut self.handler;
        surface.paint(|target| {
            let mut canvas = D2dCanvas::new(target, assets, resources);
            handler.on_paint(&info, &mut canvas);
            paint_caption_buttons(&mut canvas, width, chrome_state, &tokens);
            canvas.finish()
        })
    }

    /// Rebuilds every device resource after a device loss; the next paint redraws.
    fn recover_graphics(&mut self) {
        let Some(old) = self.graphics.take() else {
            return;
        };
        let Graphics {
            devices,
            surface,
            resources,
            assets,
        } = old;
        // The composition target must be released before a new one is made for this HWND.
        drop(surface);
        drop(resources);
        let Ok(devices) = devices.recreate() else {
            return;
        };
        let (width, height) = (self.size_px.0.max(1) as u32, self.size_px.1.max(1) as u32);
        let Ok(surface) = WindowSurface::new(&devices, self.hwnd, width, height, self.dpi as f32)
        else {
            return;
        };
        let resources = TargetResources::new(devices.d2d_factory().clone(), devices.generation());
        self.graphics = Some(Graphics {
            devices,
            surface,
            resources,
            assets,
        });
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> (Option<LRESULT>, After) {
        let hwnd = self.hwnd;
        let info = self.info();
        if let Some(result) = self.handler.on_message(&info, msg, wparam, lparam) {
            return (Some(result), After::None);
        }
        let point_lparam = |lparam: LPARAM| POINT {
            x: i32::from((lparam.0 & 0xFFFF) as i16),
            y: i32::from(((lparam.0 >> 16) & 0xFFFF) as i16),
        };
        let client_dip = |this: &Self, lparam: LPARAM| {
            let point = point_lparam(lparam);
            point_px_to_dip(point.x, point.y, this.scale())
        };
        match msg {
            WM_ERASEBKGND => (Some(LRESULT(1)), After::None),
            WM_PAINT => {
                // SAFETY: validates our own window so WM_PAINT stops repeating.
                unsafe {
                    let _ = ValidateRect(Some(hwnd), None);
                }
                self.paint();
                (Some(LRESULT(0)), After::None)
            }
            WM_SIZE => {
                if wparam.0 as u32 != SIZE_MINIMIZED {
                    let width = i32::from((lparam.0 & 0xFFFF) as i16);
                    let height = i32::from(((lparam.0 >> 16) & 0xFFFF) as i16);
                    self.resize(width, height);
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_GETMINMAXINFO => {
                let (width, height) = min_track_size_px(self.dpi);
                // SAFETY: for WM_GETMINMAXINFO `lparam` points to a MINMAXINFO.
                if let Some(info) = unsafe { (lparam.0 as *mut MINMAXINFO).as_mut() } {
                    info.ptMinTrackSize = POINT {
                        x: width,
                        y: height,
                    };
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_DPICHANGED => {
                self.dpi = dpi_from_wparam(wparam.0);
                // SAFETY: for WM_DPICHANGED `lparam` points to the suggested RECT.
                let suggested = unsafe { (lparam.0 as *const RECT).as_ref() }.copied();
                (
                    Some(LRESULT(0)),
                    suggested.map_or(After::None, After::SetRect),
                )
            }
            WM_NCCALCSIZE if wparam.0 != 0 => {
                // SAFETY: with `wparam != 0`, `lparam` points to NCCALCSIZE_PARAMS.
                let Some(params) = (unsafe { (lparam.0 as *mut NCCALCSIZE_PARAMS).as_mut() })
                else {
                    return (None, After::None);
                };
                let original_top = params.rgrc[0].top;
                // SAFETY: default processing computes the standard frame insets.
                let result = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
                if result.0 != 0 {
                    return (Some(result), After::None);
                }
                // Give the caption back to the client area.
                // SAFETY: the same non-null `NCCALCSIZE_PARAMS` as above; the earlier reference
                // is no longer used.
                let params = unsafe { &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS) };
                params.rgrc[0].top = original_top;
                // SAFETY: plain state query.
                if unsafe { IsZoomed(hwnd) }.as_bool() {
                    // A maximized window overhangs the monitor by its frame.
                    let dpi = self.dpi;
                    // SAFETY: pure metric queries.
                    params.rgrc[0].top += unsafe {
                        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                            + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
                    };
                    let mut bar = APPBARDATA {
                        cbSize: size_of::<APPBARDATA>() as u32,
                        ..Default::default()
                    };
                    // SAFETY: valid APPBARDATA with its size set.
                    let state = unsafe { SHAppBarMessage(ABM_GETSTATE, &mut bar) } as u32;
                    if state & ABS_AUTOHIDE != 0 {
                        params.rgrc[0].bottom -= 1;
                    }
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_NCHITTEST => {
                // Left, right and bottom borders are still non-client: ask USER first.
                // SAFETY: default processing with the original arguments.
                let default = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
                if default.0 != HTCLIENT as isize {
                    return (Some(default), After::None);
                }
                let point = self.client_point(point_lparam(lparam));
                let info = self.info();
                let region = if !info.maximized
                    && let Some(region) = top_resize_region(
                        point,
                        info.size_dip().width,
                        px_to_dip_band(self.dpi, self.scale()),
                    ) {
                    region
                } else {
                    self.handler.hit_test(point, &info)
                };
                (Some(LRESULT(region.hit_code() as isize)), After::None)
            }
            WM_NCACTIVATE => {
                let active = wparam.0 != 0;
                if self.chrome.active != active {
                    self.chrome.active = active;
                    self.invalidate();
                }
                // lParam = -1: do not repaint a (non-existent) system caption.
                // SAFETY: default processing with a documented lParam.
                let result = unsafe { DefWindowProcW(hwnd, msg, wparam, LPARAM(-1)) };
                (Some(result), After::None)
            }
            WM_ACTIVATE => {
                let active = (wparam.0 & 0xFFFF) as u32 != WA_INACTIVE;
                self.chrome.active = active;
                let info = self.info();
                self.handler.on_activate(&info, active);
                self.invalidate();
                (None, After::None)
            }
            WM_SETTINGCHANGE => {
                let setting = if lparam.0 == 0 {
                    String::new()
                } else {
                    // SAFETY: for WM_SETTINGCHANGE `lparam` is null or a NUL-terminated string.
                    unsafe { PCWSTR(lparam.0 as *const u16).to_string() }.unwrap_or_default()
                };
                if is_theme_change(&setting) {
                    let system = read_system_theme(&Win32Settings);
                    apply_dark_mode(hwnd, system.dark);
                    self.theme = system.theme;
                    let info = self.info();
                    self.handler.on_theme_changed(&info, &system);
                    self.invalidate();
                }
                (None, After::None)
            }
            WM_SETCURSOR => {
                if (lparam.0 & 0xFFFF) as u32 == HTCLIENT {
                    // SAFETY: loads and sets a stock cursor.
                    unsafe {
                        if let Ok(cursor) = LoadCursorW(None, cursor_resource(self.cursor)) {
                            SetCursor(Some(cursor));
                        }
                    }
                    (Some(LRESULT(1)), After::None)
                } else {
                    (None, After::None)
                }
            }
            WM_NCMOUSEMOVE => {
                self.track(true);
                let hit = (wparam.0 & 0xFFFF) as u32;
                self.set_hover(button_for_hit(hit));
                (None, After::None)
            }
            WM_NCMOUSELEAVE => {
                self.tracking_nonclient = false;
                self.set_hover(None);
                (Some(LRESULT(0)), After::None)
            }
            WM_NCLBUTTONDOWN => {
                let hit = (wparam.0 & 0xFFFF) as u32;
                let button = button_for_hit(hit);
                if button.is_some() {
                    // Swallow: USER would start its own tracking loop for caption buttons.
                    self.chrome.pressed = button;
                    self.chrome.hover = button;
                    self.invalidate();
                    (Some(LRESULT(0)), After::None)
                } else {
                    (None, After::None)
                }
            }
            WM_NCLBUTTONUP => {
                let hit = (wparam.0 & 0xFFFF) as u32;
                let released = button_for_hit(hit);
                let pressed = self.chrome.pressed.take();
                self.invalidate();
                match (pressed, released) {
                    (Some(pressed), Some(released)) if pressed == released => (
                        Some(LRESULT(0)),
                        match released {
                            CaptionButton::Minimize => After::Show(SW_MINIMIZE.0),
                            CaptionButton::Maximize => {
                                // SAFETY: plain state query.
                                if unsafe { IsZoomed(hwnd) }.as_bool() {
                                    After::Show(SW_RESTORE.0)
                                } else {
                                    After::Show(SW_MAXIMIZE.0)
                                }
                            }
                            CaptionButton::Close => After::Close,
                        },
                    ),
                    (Some(_), _) => (Some(LRESULT(0)), After::None),
                    _ => (None, After::None),
                }
            }
            WM_MOUSELEAVE => {
                self.tracking_client = false;
                let position = Point::new(-1.0, -1.0);
                self.pointer(PointerEventKind::Leave, None, position, PointerType::Mouse);
                (Some(LRESULT(0)), After::None)
            }
            WM_MOUSEMOVE => {
                self.track(false);
                let position = client_dip(self, lparam);
                self.pointer(PointerEventKind::Move, None, position, PointerType::Mouse);
                (Some(LRESULT(0)), After::None)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_LBUTTONUP
            | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
                let down = matches!(
                    msg,
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                );
                let button = match msg {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => PointerButton::Left,
                    WM_RBUTTONDOWN | WM_RBUTTONUP => PointerButton::Right,
                    WM_MBUTTONDOWN | WM_MBUTTONUP => PointerButton::Middle,
                    _ if (wparam.0 >> 16) & 0xFFFF == 1 => PointerButton::X1,
                    _ => PointerButton::X2,
                };
                let position = client_dip(self, lparam);
                let kind = if down {
                    PointerEventKind::Down
                } else {
                    PointerEventKind::Up
                };
                self.pointer(kind, Some(button), position, PointerType::Mouse);
                let result = if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                    1
                } else {
                    0
                };
                (Some(LRESULT(result)), After::None)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let position = self.client_point(point_lparam(lparam));
                let event =
                    wheel_event(delta, msg == WM_MOUSEHWHEEL, position, current_modifiers());
                self.dispatch(Event::Wheel(event));
                (Some(LRESULT(0)), After::None)
            }
            WM_POINTERDOWN | WM_POINTERUP | WM_POINTERUPDATE | WM_POINTERLEAVE => {
                let id = (wparam.0 & 0xFFFF) as u32;
                let mut kind = windows::Win32::UI::WindowsAndMessaging::POINTER_INPUT_TYPE(0);
                // SAFETY: valid out pointers for a pointer id delivered with this message.
                if unsafe { GetPointerType(id, &mut kind) }.is_err() {
                    return (None, After::None);
                }
                let pointer_type = match kind {
                    windows::Win32::UI::WindowsAndMessaging::PT_TOUCH => PointerType::Touch,
                    windows::Win32::UI::WindowsAndMessaging::PT_PEN => PointerType::Pen,
                    windows::Win32::UI::WindowsAndMessaging::PT_TOUCHPAD => PointerType::Touchpad,
                    // The mouse is handled through WM_MOUSE* messages.
                    _ => return (None, After::None),
                };
                let mut info = POINTER_INFO::default();
                // SAFETY: valid out pointer.
                if unsafe { GetPointerInfo(id, &mut info) }.is_err() {
                    return (None, After::None);
                }
                let position = self.client_point(info.ptPixelLocation);
                let button = if info.pointerFlags.0 & POINTER_FLAG_SECONDBUTTON != 0 {
                    PointerButton::Right
                } else {
                    PointerButton::Left
                };
                match msg {
                    WM_POINTERDOWN => {
                        self.pointer(PointerEventKind::Down, Some(button), position, pointer_type);
                    }
                    WM_POINTERUP => {
                        self.pointer(PointerEventKind::Up, Some(button), position, pointer_type);
                    }
                    WM_POINTERUPDATE => {
                        self.pointer(PointerEventKind::Move, None, position, pointer_type);
                    }
                    _ => self.pointer(PointerEventKind::Leave, None, position, pointer_type),
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
                let is_down = matches!(msg, WM_KEYDOWN | WM_SYSKEYDOWN);
                let event = key_event(
                    (wparam.0 & 0xFFFF) as u16,
                    lparam.0,
                    is_down,
                    current_modifiers(),
                );
                self.dispatch(Event::Key(event));
                // System keys (Alt+F4, Alt+Space) keep their default behavior.
                if matches!(msg, WM_SYSKEYDOWN | WM_SYSKEYUP) {
                    (None, After::None)
                } else {
                    (Some(LRESULT(0)), After::None)
                }
            }
            WM_CHAR => {
                let unit = (wparam.0 & 0xFFFF) as u16;
                if let Some(text) = text_from_char(unit, &mut self.pending_high_surrogate) {
                    self.dispatch(Event::Text(TextInput(text)));
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_SETFOCUS => {
                self.dispatch(Event::Focus(FocusEvent::Gained));
                (Some(LRESULT(0)), After::None)
            }
            WM_KILLFOCUS => {
                self.dispatch(Event::Focus(FocusEvent::Lost));
                (Some(LRESULT(0)), After::None)
            }
            WM_TIMER => {
                let fired = self.timers.borrow_mut().fire(wparam.0);
                if let Some(fired) = fired {
                    if fired.one_shot {
                        // SAFETY: kills the one-shot timer of our own window.
                        unsafe {
                            let _ = windows::Win32::UI::WindowsAndMessaging::KillTimer(
                                Some(hwnd),
                                wparam.0,
                            );
                        }
                    }
                    self.dispatch(Event::Timer(fired.id));
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_WAKE => {
                let info = self.info();
                self.handler.on_wake(&info);
                (Some(LRESULT(0)), After::None)
            }
            WM_CLOSE => {
                let info = self.info();
                if self.handler.on_close_requested(&info) {
                    (None, After::None)
                } else {
                    (Some(LRESULT(0)), After::None)
                }
            }
            WM_DESTROY => {
                let info = self.info();
                self.handler.on_destroy(&info);
                self.graphics = None;
                (Some(LRESULT(0)), After::None)
            }
            _ => (None, After::None),
        }
    }
}

/// Barrel / second-button flag of `POINTER_INFO::pointerFlags`.
const POINTER_FLAG_SECONDBUTTON: u32 = 0x0000_0020;

fn px_to_dip_band(dpi: u32, scale: f32) -> f32 {
    top_resize_band_px(dpi) as f32 / scale
}

/// The window procedure of [`WindowHost`] windows. Nothing may unwind out of it (the OS
/// callback frames above it cannot be unwound through): a panic in the handler is caught,
/// parked for [`rethrow_pending_panic`] and the message gets default processing.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| dispatch(hwnd, msg, wparam, lparam))) {
        Ok(result) => result,
        Err(payload) => {
            stash_panic(payload);
            // SAFETY: default processing with the original arguments.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

/// Routes one message to the window's [`WindowState`] (or to default processing).
fn dispatch(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: for WM_NCCREATE `lparam` points to the CREATESTRUCTW whose creation
        // parameter is the `RefCell<WindowState>` pointer passed by `WindowHost::create`.
        unsafe {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            let cell = &*(create.lpCreateParams as *const RefCell<WindowState>);
            if let Ok(mut state) = cell.try_borrow_mut() {
                state.hwnd = hwnd;
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    }
    // SAFETY: the pointer was stored in WM_NCCREATE and is cleared in WM_NCDESTROY; the cell
    // outlives the window because `WindowHost` destroys the window before dropping it.
    let cell =
        unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<WindowState>).as_ref() };
    let Some(cell) = cell else {
        // SAFETY: default processing.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    if msg == WM_NCDESTROY {
        // SAFETY: detaches the state; later messages take the default path.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
        // SAFETY: default processing.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // A message sent while another one is being handled (re-entrancy through a callback)
    // takes the default path instead of aliasing the state.
    let (result, after) = match cell.try_borrow_mut() {
        Ok(mut state) => state.handle(msg, wparam, lparam),
        Err(_) => (None, After::None),
    };
    match after {
        After::None => {}
        After::SetRect(rect) => {
            // SAFETY: the borrow is released; this sends WM_SIZE / WM_WINDOWPOSCHANGED back.
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        After::Close => {
            // SAFETY: posts WM_CLOSE to our own window.
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        After::Show(command) => {
            // SAFETY: shows our own window.
            unsafe {
                let _ = ShowWindow(
                    hwnd,
                    windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD(command),
                );
            }
        }
    }
    match result {
        Some(result) => result,
        // SAFETY: default processing with the original arguments.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// An application window.
pub struct WindowHost {
    hwnd: HWND,
    state: Rc<RefCell<WindowState>>,
}

impl std::fmt::Debug for WindowHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowHost")
            .field("hwnd", &self.hwnd)
            .finish_non_exhaustive()
    }
}

impl WindowHost {
    /// Creates the (hidden) window with the custom title bar, Mica Alt backdrop and rounded
    /// corners.
    ///
    /// # Errors
    /// Fails when the window cannot be created.
    pub fn create(
        options: &WindowOptions,
        handler: Box<dyn WindowHandler>,
        theme: Theme,
    ) -> Result<Self, WinError> {
        register_classes()?;
        let timers = Rc::new(RefCell::new(TimerTable::default()));
        let state = Rc::new(RefCell::new(WindowState {
            hwnd: HWND::default(),
            handler,
            dpi: DEFAULT_DPI,
            size_px: (0, 0),
            chrome: ChromeState::default(),
            theme,
            graphics: None,
            pending_high_surrogate: None,
            clicks: ClickTracker::new(
                // SAFETY: pure query.
                Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() })),
                4.0,
            ),
            started: Instant::now(),
            cursor: Cursor::Arrow,
            tracking_client: false,
            tracking_nonclient: false,
            timers,
            painting: false,
        }));
        let (x, y, width, height) =
            options
                .rect
                .unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT, CW_USEDEFAULT));
        let mut ex_style = WS_EX_APPWINDOW | WS_EX_NOREDIRECTIONBITMAP;
        if options.no_activate {
            ex_style |= WS_EX_NOACTIVATE;
        }
        let title = HSTRING::from(options.title.as_str());
        // SAFETY: the creation parameter is the address of the `RefCell` inside the `Rc`,
        // which `WindowHost` keeps alive until after the window is destroyed.
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                CLASS_NAME,
                &title,
                WS_OVERLAPPEDWINDOW,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(instance()),
                Some(Rc::as_ptr(&state).cast()),
            )
        }
        .ctx("CreateWindowExW")?;
        // From here on `host` owns the window: every early return below destroys it, so the
        // window procedure can never meet the state pointer after the `Rc` is gone.
        let host = Self { hwnd, state };
        let dpi = {
            // SAFETY: plain query on the window just created.
            let dpi = unsafe { GetDpiForWindow(hwnd) };
            if dpi == 0 { DEFAULT_DPI } else { dpi }
        };
        let mut client = RECT::default();
        // SAFETY: valid out pointer.
        unsafe { GetClientRect(hwnd, &mut client) }.ctx("GetClientRect")?;
        {
            let mut borrowed = host.state.borrow_mut();
            borrowed.hwnd = hwnd;
            borrowed.dpi = dpi;
            borrowed.size_px = (client.right - client.left, client.bottom - client.top);
        }
        apply_dark_mode(hwnd, options.dark);
        set_dwm_i32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE.0, DWMSBT_TABBEDWINDOW.0);
        set_dwm_i32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE.0, DWMWCP_ROUND.0);
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        // SAFETY: valid window and margins; re-running NCCALCSIZE applies the custom frame.
        unsafe {
            DwmExtendFrameIntoClientArea(hwnd, &margins).ctx("DwmExtendFrameIntoClientArea")?;
            SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .ctx("SetWindowPos")?;
        }
        let mut client = RECT::default();
        // SAFETY: valid out pointer.
        unsafe { GetClientRect(hwnd, &mut client) }.ctx("GetClientRect")?;
        host.state.borrow_mut().size_px = (client.right - client.left, client.bottom - client.top);
        Ok(host)
    }

    /// Creates the swap chain, D2D resources and canvas assets; the window renders from now on.
    ///
    /// # Errors
    /// Fails when the composition surface cannot be created.
    pub fn attach_graphics(
        &self,
        devices: GfxDevices,
        assets: CanvasAssets,
    ) -> Result<(), WinError> {
        let mut state = self.state.borrow_mut();
        let (width, height) = (state.size_px.0.max(1) as u32, state.size_px.1.max(1) as u32);
        let surface = WindowSurface::new(&devices, self.hwnd, width, height, state.dpi as f32)?;
        let resources = TargetResources::new(devices.d2d_factory().clone(), devices.generation());
        state.graphics = Some(Graphics {
            devices,
            surface,
            resources,
            assets,
        });
        Ok(())
    }

    #[must_use]
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// A thread-safe handle that wakes the UI thread.
    #[must_use]
    pub fn wake_handle(&self) -> WakeHandle {
        WakeHandle {
            hwnd: self.hwnd.0 as isize,
        }
    }

    /// The timers bound to this window.
    #[must_use]
    pub fn timers(&self) -> crate::services::timers::WindowTimers {
        crate::services::timers::WindowTimers::new(
            self.hwnd,
            Rc::clone(&self.state.borrow().timers),
        )
    }

    /// Current window facts.
    #[must_use]
    pub fn info(&self) -> WindowInfo {
        self.state.borrow().info()
    }

    /// Replaces the theme used for the caption buttons and repaints.
    pub fn set_theme(&self, theme: Theme) {
        let mut state = self.state.borrow_mut();
        state.theme = theme;
        state.invalidate();
    }

    /// Sets the cursor shown over the client area.
    pub fn set_cursor(&self, cursor: Cursor) {
        self.state.borrow_mut().cursor = cursor;
    }

    /// Schedules a repaint.
    pub fn invalidate(&self) {
        self.state.borrow().invalidate();
    }

    /// Mutable access to the canvas assets (image and scene registration); `None` before
    /// [`WindowHost::attach_graphics`].
    pub fn with_assets<R>(&self, f: impl FnOnce(&mut CanvasAssets) -> R) -> Option<R> {
        let mut state = self.state.borrow_mut();
        state
            .graphics
            .as_mut()
            .map(|graphics| f(&mut graphics.assets))
    }

    /// The generation of the graphics devices in use.
    #[must_use]
    pub fn graphics_generation(&self) -> Option<u64> {
        self.state
            .borrow()
            .graphics
            .as_ref()
            .map(|graphics| graphics.devices.generation())
    }

    /// Delivers a pending animation-frame request as `Event::AnimationFrame`; returns whether
    /// one was delivered.
    pub fn deliver_animation_frame(&self) -> bool {
        let mut state = self.state.borrow_mut();
        if !state.timers.borrow_mut().take_frame_request() {
            return false;
        }
        let time = state.started.elapsed();
        state.dispatch(Event::AnimationFrame(time));
        true
    }

    /// Shows the window without activating it.
    pub fn show_no_activate(&self) {
        // SAFETY: shows our own window.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
        }
    }

    /// The screen position of the client point (`x`, `y`) in pixels.
    #[must_use]
    pub fn client_to_screen(&self, x: i32, y: i32) -> (i32, i32) {
        let mut point = POINT { x, y };
        // SAFETY: converts using our own window handle; a failure leaves the point unchanged.
        let _ = unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(self.hwnd, &mut point) };
        (point.x, point.y)
    }

    /// Restores a minimized window and brings it to the foreground (a launch forwarded by a
    /// later process asks for it).
    pub fn bring_to_front(&self) {
        // SAFETY: state queries and show / foreground requests for our own window; the system
        // may refuse the foreground change, which is not an error.
        unsafe {
            if windows::Win32::UI::WindowsAndMessaging::IsIconic(self.hwnd).as_bool() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(self.hwnd);
        }
    }

    /// Shows the window maximized (activating it).
    pub fn show_maximized(&self) {
        // SAFETY: shows our own window.
        unsafe {
            let _ = ShowWindow(
                self.hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWMAXIMIZED,
            );
        }
    }

    /// The restored position and size of the window in pixels (`x`, `y`, `width`, `height`),
    /// also while it is maximized or minimized: what a session stores.
    #[must_use]
    pub fn normal_placement(&self) -> Option<(i32, i32, i32, i32)> {
        let mut placement = windows::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT {
            length: size_of::<windows::Win32::UI::WindowsAndMessaging::WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        // SAFETY: a valid out structure with its length set, for our own window.
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowPlacement(self.hwnd, &mut placement)
        }
        .ok()?;
        let rect = placement.rcNormalPosition;
        Some((
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        ))
    }

    /// Shows the window normally (activating it).
    pub fn show(&self) {
        // SAFETY: shows our own window.
        unsafe {
            let _ = ShowWindow(self.hwnd, windows::Win32::UI::WindowsAndMessaging::SW_SHOW);
        }
    }

    /// Destroys the window now (the close request is not consulted).
    ///
    /// Safe to call again once the window is gone: the handle value may already belong to
    /// another window by then, so only a window whose user data still is this host's state
    /// is destroyed (`WM_NCDESTROY` clears it).
    pub fn destroy(&self) {
        // SAFETY: a plain query, then the destruction of our own window on its owning thread.
        unsafe {
            let ours =
                GetWindowLongPtrW(self.hwnd, GWLP_USERDATA) == Rc::as_ptr(&self.state) as isize;
            if ours {
                let _ = DestroyWindow(self.hwnd);
            }
        }
    }
}

impl Drop for WindowHost {
    fn drop(&mut self) {
        self.destroy();
        // `on_destroy` may have panicked inside the procedure; surface it here unless this
        // drop already runs because of another panic.
        if !std::thread::panicking() {
            rethrow_pending_panic();
        }
    }
}

/// Posts `WM_QUIT` so [`run_loop`] returns `code`.
pub fn quit(code: i32) {
    // SAFETY: posts WM_QUIT to the calling thread's queue.
    unsafe { PostQuitMessage(code) };
}

/// Pumps messages until `WM_QUIT`; returns its exit code.
///
/// Whenever the queue is empty `idle` runs (deliver animation frames, flush work) and returns
/// how long the thread may sleep: `Some(duration)` wakes at the latest after it, `None` waits
/// for the next message (workers wake the loop with [`WakeHandle::wake`]).
pub fn run_loop(mut idle: impl FnMut() -> Option<Duration>) -> i32 {
    loop {
        let mut message = MSG::default();
        // SAFETY: standard message pump on the calling thread with a valid MSG buffer.
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            if message.message == WM_QUIT {
                return message.wParam.0 as i32;
            }
            // SAFETY: dispatches the message this thread just retrieved.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            // A handler panic was parked by the window procedure: raise it here, outside of
            // the OS callback.
            rethrow_pending_panic();
        }
        let timeout = idle().map_or(u32::MAX, |wait| {
            u32::try_from(wait.as_millis().max(1)).unwrap_or(u32::MAX - 1)
        });
        // SAFETY: waits for input without handles; `MWMO_INPUTAVAILABLE` returns for messages
        // that arrived before the call.
        unsafe {
            MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
}

/// The number of lines a wheel notch scrolls (`SPI_GETWHEELSCROLLLINES`); 3 when unavailable.
#[must_use]
pub fn wheel_scroll_lines() -> u32 {
    let mut lines = 3u32;
    // SAFETY: `lines` is a valid u32 out buffer.
    let ok = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&mut lines as *mut u32).cast()),
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_ok() { lines } else { 3 }
}

/// Pixel size of a DIP length at the window's DPI.
#[must_use]
pub fn dip_len_px(info: &WindowInfo, dip: f32) -> i32 {
    dip_to_px(dip, info.scale())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssw_ui::canvas::DrawOp;
    use ssw_ui::geometry::Rect;
    use ssw_ui::theme::DEFAULT_ACCENT;
    use std::cell::RefCell as Cell;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, SendMessageW};

    #[derive(Default)]
    struct Log {
        events: Vec<Event>,
        closes: u32,
        destroys: u32,
        wakes: u32,
        resizes: Vec<(i32, i32)>,
        activations: Vec<bool>,
        themes: u32,
        paints: u32,
    }

    struct TestHandler {
        log: Rc<Cell<Log>>,
        allow_close: bool,
        custom_hits: bool,
    }

    impl WindowHandler for TestHandler {
        fn hit_test(&self, point: Point, info: &WindowInfo) -> HitRegion {
            if self.custom_hits && point.x < 50.0 && point.y >= 100.0 && point.y < 120.0 {
                return HitRegion::Resize(ResizeEdge::Left);
            }
            default_hit_region(point, info.size_dip().width)
        }
        fn on_event(&mut self, _info: &WindowInfo, event: Event) {
            self.log.borrow_mut().events.push(event);
        }
        fn on_paint(&mut self, _info: &WindowInfo, canvas: &mut dyn Canvas) {
            self.log.borrow_mut().paints += 1;
            canvas.fill_rect(
                Rect::new(0.0, 0.0, 10.0, 10.0),
                ssw_ui::geometry::Color::BLACK,
            );
        }
        fn on_resize(&mut self, info: &WindowInfo) {
            self.log.borrow_mut().resizes.push(info.size_px);
        }
        fn on_activate(&mut self, _info: &WindowInfo, active: bool) {
            self.log.borrow_mut().activations.push(active);
        }
        fn on_theme_changed(&mut self, _info: &WindowInfo, _theme: &SystemTheme) {
            self.log.borrow_mut().themes += 1;
        }
        fn on_wake(&mut self, _info: &WindowInfo) {
            self.log.borrow_mut().wakes += 1;
        }
        fn on_close_requested(&mut self, _info: &WindowInfo) -> bool {
            self.log.borrow_mut().closes += 1;
            self.allow_close
        }
        fn on_destroy(&mut self, _info: &WindowInfo) {
            self.log.borrow_mut().destroys += 1;
        }
    }

    fn host(allow_close: bool, custom_hits: bool) -> (WindowHost, Rc<Cell<Log>>) {
        let log = Rc::new(Cell::new(Log::default()));
        let options = WindowOptions {
            title: "test".to_owned(),
            rect: Some((-32000, -32000, 800, 600)),
            dark: true,
            no_activate: true,
        };
        let handler = Box::new(TestHandler {
            log: Rc::clone(&log),
            allow_close,
            custom_hits,
        });
        let host = WindowHost::create(&options, handler, Theme::light(DEFAULT_ACCENT)).unwrap();
        (host, log)
    }

    fn send(host: &WindowHost, msg: u32, wparam: usize, lparam: isize) -> isize {
        // SAFETY: synchronous message to a window owned by this thread.
        let result =
            unsafe { SendMessageW(host.hwnd(), msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))) }.0;
        // A panic of the handler was parked by the window procedure: raise it in the test.
        rethrow_pending_panic();
        result
    }

    /// Feeds a message straight to the window's state, bypassing the window procedure and with
    /// it default processing (which would, for example, move keyboard focus).
    fn feed(host: &WindowHost, msg: u32, wparam: usize, lparam: isize) -> (Option<isize>, After) {
        let (result, after) = host
            .state
            .borrow_mut()
            .handle(msg, WPARAM(wparam), LPARAM(lparam));
        (result.map(|result| result.0), after)
    }

    fn hit(host: &WindowHost, x: i32, y: i32) -> u32 {
        let mut point = POINT { x, y };
        // SAFETY: converts a client point of our window to screen coordinates.
        let _ = unsafe { ClientToScreen(host.hwnd(), &mut point) };
        let lparam = ((point.y as isize & 0xFFFF) << 16) | (point.x as isize & 0xFFFF);
        send(host, WM_NCHITTEST, 0, lparam) as u32
    }

    #[test]
    fn hit_testing_covers_caption_buttons_and_edges() {
        let (host, _log) = host(true, true);
        let info = host.info();
        assert_eq!(info.dpi, crate::dpi::window_dpi(host.hwnd()));
        let width = info.size_px.0;
        assert!(width > 400, "client width {width}");
        let scale = info.scale();
        let width_dip = width as f32 / scale;
        // Pixel x inside caption button `n` (1 = close, 2 = maximize, 3 = minimize).
        let button = |n: i32| ((width_dip - 46.0 * n as f32 + 20.0) * scale) as i32;
        assert_eq!(hit(&host, button(1), 10), HTCLOSE);
        assert_eq!(hit(&host, button(2), 10), HTMAXBUTTON);
        assert_eq!(hit(&host, button(3), 10), HTMINBUTTON);
        assert_eq!(hit(&host, 200, 10), HTCAPTION);
        assert_eq!(hit(&host, 200, 200), HTCLIENT);
        assert_eq!(hit(&host, 20, 110), HTLEFT);
        assert_eq!(hit(&host, 200, 1), HTTOP);
        assert_eq!(hit(&host, 1, 1), HTTOPLEFT);
        assert_eq!(hit(&host, width - 2, 1), HTTOPRIGHT);
        let height = info.size_px.1;
        assert_eq!(hit(&host, 200, height + 3), HTBOTTOM);
    }

    #[test]
    fn caption_button_clicks_run_their_actions() {
        let (host, log) = host(false, false);
        let ncl_button_down = |code: u32| send(&host, WM_NCLBUTTONDOWN, code as usize, 0);
        let ncl_button_up = |code: u32| send(&host, WM_NCLBUTTONUP, code as usize, 0);
        // Close: the request goes through the handler, which refuses.
        assert_eq!(ncl_button_down(HTCLOSE), 0);
        assert_eq!(
            host.state.borrow().chrome.pressed,
            Some(CaptionButton::Close)
        );
        ncl_button_up(HTCLOSE);
        pump();
        assert_eq!(log.borrow().closes, 1);
        assert!(host.state.borrow().chrome.pressed.is_none());
        // Release on a different button cancels the press.
        ncl_button_down(HTMINBUTTON);
        ncl_button_up(HTCLOSE);
        pump();
        assert_eq!(log.borrow().closes, 1);
        // Hover tracking.
        send(&host, WM_NCMOUSEMOVE, HTMAXBUTTON as usize, 0);
        assert_eq!(
            host.state.borrow().chrome.hover,
            Some(CaptionButton::Maximize)
        );
        send(&host, WM_NCMOUSEMOVE, HTCAPTION as usize, 0);
        assert_eq!(host.state.borrow().chrome.hover, None);
        send(&host, WM_NCMOUSELEAVE, 0, 0);
        assert!(!host.state.borrow().tracking_nonclient);
        assert!(host.state.borrow().chrome.pressed.is_none());
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

    #[test]
    fn input_messages_become_events() {
        let (host, log) = host(true, false);
        send(&host, WM_KEYDOWN, usize::from(vk::RETURN), 0x001C_0001);
        send(
            &host,
            WM_KEYUP,
            usize::from(vk::RETURN),
            0xC01C_0001u32 as isize,
        );
        send(&host, WM_CHAR, 'ç' as usize, 1);
        send(&host, WM_CHAR, 13, 1);
        send(&host, WM_CHAR, 0xD83D, 1);
        send(&host, WM_CHAR, 0xDE00, 1);
        // Focus messages never go through the real procedure: a synthetic `WM_SETFOCUS` has no
        // business in a live window. The state answers them itself, without default processing.
        for msg in [WM_SETFOCUS, WM_KILLFOCUS] {
            let (result, after) = feed(&host, msg, 0, 0);
            assert_eq!(result, Some(0));
            assert!(matches!(after, After::None));
        }
        let wheel = (120u32 as isize) << 16;
        send(&host, WM_MOUSEWHEEL, (wheel as usize) & !0xFFFF, 0);
        send(&host, WM_MOUSEHWHEEL, ((-60i16) as u16 as usize) << 16, 0);
        send(&host, WM_LBUTTONDOWN, 1, (20 << 16) | 10);
        send(&host, WM_LBUTTONDOWN, 1, (20 << 16) | 10);
        send(&host, WM_LBUTTONUP, 0, (20 << 16) | 10);
        send(&host, WM_RBUTTONDOWN, 2, 0);
        send(&host, WM_MBUTTONUP, 0, 0);
        send(&host, WM_XBUTTONDOWN, 0x0001_0000, 0);
        send(&host, WM_XBUTTONUP, 0x0002_0000, 0);
        send(&host, WM_MOUSEMOVE, 0, (8 << 16) | 4);
        send(&host, WM_MOUSELEAVE, 0, 0);
        let log = log.borrow();
        let events = &log.events;
        assert!(matches!(
            events[0],
            Event::Key(KeyEvent {
                vk: 0x0D,
                scan: 0x1C,
                is_down: true,
                repeat: false,
                ..
            })
        ));
        assert!(matches!(
            events[1],
            Event::Key(KeyEvent {
                is_down: false,
                repeat: false,
                ..
            })
        ));
        assert_eq!(events[2], Event::Text(TextInput("ç".to_owned())));
        assert_eq!(events[3], Event::Text(TextInput("😀".to_owned())));
        assert_eq!(events[4], Event::Focus(FocusEvent::Gained));
        assert_eq!(events[5], Event::Focus(FocusEvent::Lost));
        assert!(matches!(&events[6], Event::Wheel(w) if w.delta.y == 1.0 && !w.precise));
        assert!(matches!(&events[7], Event::Wheel(w) if w.delta.x == -0.5));
        let clicks: Vec<u8> = events
            .iter()
            .filter_map(|event| match event {
                Event::Pointer(p) if p.kind == PointerEventKind::Down => Some(p.click_count),
                _ => None,
            })
            .collect();
        assert_eq!(&clicks[..2], &[1, 2], "double click");
        let buttons: Vec<Option<PointerButton>> = events
            .iter()
            .filter_map(|event| match event {
                Event::Pointer(p)
                    if p.kind != PointerEventKind::Move && p.kind != PointerEventKind::Leave =>
                {
                    Some(p.button)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            buttons,
            vec![
                Some(PointerButton::Left),
                Some(PointerButton::Left),
                Some(PointerButton::Left),
                Some(PointerButton::Right),
                Some(PointerButton::Middle),
                Some(PointerButton::X1),
                Some(PointerButton::X2),
            ]
        );
        assert!(
            events.iter().any(
                |event| matches!(event, Event::Pointer(p) if p.kind == PointerEventKind::Move)
            )
        );
        assert!(
            matches!(events.last(), Some(Event::Pointer(p)) if p.kind == PointerEventKind::Leave)
        );
        let down = events.iter().find_map(|event| match event {
            Event::Pointer(p) if p.kind == PointerEventKind::Down => Some(p.position),
            _ => None,
        });
        assert_eq!(
            down,
            Some(Point::new(
                10.0 / host.info().scale(),
                20.0 / host.info().scale()
            ))
        );
    }

    #[test]
    fn lifecycle_messages_reach_the_handler() {
        let (host, log) = host(true, false);
        // Activation messages are fed to the state directly: sent to the real procedure, the
        // default processing of a synthetic `WM_ACTIVATE` could move keyboard focus. Only the
        // non-client activation (frame state, no focus change) goes through the procedure.
        let (result, after) = feed(&host, WM_ACTIVATE, usize::from(WA_INACTIVE as u16), 0);
        assert_eq!(result, None, "default processing stays with the procedure");
        assert!(matches!(after, After::None));
        assert!(!host.info().active);
        feed(&host, WM_ACTIVATE, 1, 0);
        assert!(host.info().active);
        feed(&host, WM_ACTIVATE, usize::from(WA_INACTIVE as u16), 0);
        send(&host, WM_NCACTIVATE, 1, 0);
        send(&host, WM_NCACTIVATE, 0, 0);
        assert!(!host.info().active);
        // A pointer message for a pointer that does not exist is left to default processing.
        let (result, after) = feed(&host, WM_POINTERDOWN, 0xFFFE, 0);
        assert_eq!(result, None);
        assert!(matches!(after, After::None));
        assert!(host.state.borrow().chrome.pressed.is_none());
        send(&host, WM_SETTINGCHANGE, 0, 0);
        assert_eq!(log.borrow().themes, 0);
        let setting = to_wide("ImmersiveColorSet");
        send(&host, WM_SETTINGCHANGE, 0, setting.as_ptr() as isize);
        assert_eq!(log.borrow().themes, 1);
        let other = to_wide("intl");
        send(&host, WM_SETTINGCHANGE, 0, other.as_ptr() as isize);
        assert_eq!(log.borrow().themes, 1);
        assert!(host.wake_handle().wake());
        pump();
        assert_eq!(log.borrow().wakes, 1);
        assert_eq!(send(&host, WM_ERASEBKGND, 0, 0), 1);
        assert_eq!(log.borrow().activations, vec![false, true, false]);
        host.set_cursor(Cursor::Hand);
        assert_eq!(send(&host, WM_SETCURSOR, 0, HTCLIENT as isize), 1);
        assert_eq!(send(&host, WM_SETCURSOR, 0, HTCAPTION as isize), 0);
        let mut info = MINMAXINFO::default();
        send(
            &host,
            WM_GETMINMAXINFO,
            0,
            (&mut info as *mut MINMAXINFO) as isize,
        );
        assert_eq!(info.ptMinTrackSize.x, min_track_size_px(host.info().dpi).0);
        let host_wake = host.wake_handle();
        drop(host);
        assert!(!host_wake.wake());
        assert_eq!(log.borrow().destroys, 1);
    }

    #[test]
    fn close_requests_are_negotiated() {
        let (refusing, log) = host(false, false);
        send(&refusing, WM_CLOSE, 0, 0);
        assert_eq!(log.borrow().closes, 1);
        assert!(log.borrow().destroys == 0);
        let (accepting, log) = host(true, false);
        send(&accepting, WM_CLOSE, 0, 0);
        assert_eq!(log.borrow().destroys, 1);
    }

    /// A handler that panics in `on_wake` (numbered) and, when asked, in `on_destroy`.
    struct Bomb {
        wakes: u32,
        fragile_destroy: bool,
    }

    impl WindowHandler for Bomb {
        fn on_event(&mut self, _info: &WindowInfo, _event: Event) {}
        fn on_paint(&mut self, _info: &WindowInfo, _canvas: &mut dyn Canvas) {}
        fn on_wake(&mut self, _info: &WindowInfo) {
            self.wakes += 1;
            panic!("wake {}", self.wakes);
        }
        fn on_destroy(&mut self, _info: &WindowInfo) {
            assert!(!self.fragile_destroy, "destroy failed");
        }
    }

    fn bomb_host(fragile_destroy: bool) -> WindowHost {
        let options = WindowOptions {
            title: "bomb".to_owned(),
            rect: Some((-32000, -32000, 400, 300)),
            dark: false,
            no_activate: true,
        };
        let handler = Box::new(Bomb {
            wakes: 0,
            fragile_destroy,
        });
        WindowHost::create(&options, handler, Theme::light(DEFAULT_ACCENT)).unwrap()
    }

    fn payload_text(payload: &(dyn Any + Send)) -> Option<&str> {
        payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
    }

    #[test]
    fn a_panicking_handler_is_contained_and_rethrown_outside_the_callback() {
        let host = bomb_host(false);
        // The panics stay inside the procedure: the sender gets default processing's answer
        // and the window keeps working.
        let answers = [(); 2].map(|()| {
            // SAFETY: synchronous message to a window owned by this thread.
            unsafe { SendMessageW(host.hwnd(), WM_WAKE, None, None) }.0
        });
        assert_eq!(answers, [0, 0]);
        assert_eq!(host.info().hwnd, host.hwnd());
        // Only the first panic is kept, and it is raised exactly once.
        let payload = std::panic::catch_unwind(rethrow_pending_panic).unwrap_err();
        assert_eq!(payload_text(&*payload), Some("wake 1"));
        rethrow_pending_panic();
        assert!(host.wake_handle().wake());
        // The loop raises a parked panic after the message that caused it.
        let outcome = std::panic::catch_unwind(|| run_loop(|| Some(Duration::from_millis(50))));
        assert_eq!(payload_text(&*outcome.unwrap_err()), Some("wake 3"));
    }

    #[test]
    fn a_panic_in_on_destroy_surfaces_when_the_host_is_dropped_but_never_while_unwinding() {
        let host = bomb_host(true);
        let payload = std::panic::catch_unwind(AssertUnwindSafe(move || drop(host))).unwrap_err();
        assert_eq!(payload_text(&*payload), Some("destroy failed"));
        rethrow_pending_panic();

        // Already unwinding: the parked panic must not start a second one (that would abort).
        let outer = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _host = bomb_host(true);
            panic!("outer");
        }))
        .unwrap_err();
        assert_eq!(payload_text(&*outer), Some("outer"));
        let parked = std::panic::catch_unwind(rethrow_pending_panic).unwrap_err();
        assert_eq!(payload_text(&*parked), Some("destroy failed"));
    }

    #[test]
    fn destroying_twice_leaves_a_window_that_reused_the_handle_alone() {
        let (first, first_log) = host(true, false);
        first.destroy();
        assert_eq!(first_log.borrow().destroys, 1);
        // The next window may get the same handle value; the stale host must not touch it.
        let (second, second_log) = host(true, false);
        first.destroy();
        assert_eq!(second_log.borrow().destroys, 0);
        // SAFETY: plain query.
        let alive =
            unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(second.hwnd())) };
        assert!(alive.as_bool());
    }

    #[test]
    fn message_box_arguments_are_nul_terminated_utf16_and_reach_the_presenter() {
        // The box itself is never shown here: a real message box would block on the screen.
        let owner = MessageWindow::new().unwrap();
        let mut seen = None;
        show_message_box_with(
            Some(owner.hwnd()),
            "Başlık",
            "Metin 😀",
            |hwnd, title, text| {
                seen = Some((hwnd, title.to_vec(), text.to_vec()));
            },
        );
        let (hwnd, title, text) = seen.unwrap();
        assert_eq!(hwnd, Some(owner.hwnd()));
        assert_eq!(title, to_wide("Başlık"));
        assert_eq!(text.last(), Some(&0));
        assert_eq!(crate::com::from_wide(&text), "Metin 😀");
        let mut ownerless = false;
        show_message_box_with(None, "", "", |hwnd, title, text| {
            ownerless = hwnd.is_none() && title == [0] && text == [0];
        });
        assert!(ownerless);
    }

    #[test]
    fn graphics_attach_renders_and_resizes() {
        // COM is initialized first so it is torn down last, after the window released its
        // D2D / WIC objects.
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let (host, log) = host(true, false);
        let devices = crate::gfx::DeviceStartup::spawn().wait().unwrap();
        let wic = crate::wic::Wic::new().unwrap();
        let engine = crate::dwrite::DWriteEngine::new().unwrap();
        let assets = CanvasAssets::new(crate::canvas::ImageStore::new(wic), engine);
        host.attach_graphics(devices, assets).unwrap();
        assert_eq!(host.graphics_generation(), Some(1));
        assert_eq!(host.with_assets(|assets| assets.images.len()), Some(0));
        send(&host, WM_PAINT, 0, 0);
        assert!(log.borrow().paints >= 1);
        let before = log.borrow().paints;
        // SAFETY: resizes our own off-screen window.
        unsafe {
            let _ = SetWindowPos(
                host.hwnd(),
                None,
                0,
                0,
                640,
                480,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        assert!(log.borrow().paints > before, "WM_SIZE repaints immediately");
        let last = *log.borrow().resizes.last().unwrap();
        assert_eq!(last, host.info().size_px);
        host.set_theme(Theme::dark(DEFAULT_ACCENT));
        host.invalidate();
        let mut rect = RECT::default();
        // SAFETY: valid out pointer.
        unsafe { GetWindowRect(host.hwnd(), &mut rect) }.unwrap();
        assert_eq!(rect.right - rect.left, 640);
        // DPI change: the suggested rectangle is applied.
        let suggested = RECT {
            left: -32000,
            top: -32000,
            right: -31000,
            bottom: -31200,
        };
        send(
            &host,
            WM_DPICHANGED,
            0x00C0_00C0,
            (&suggested as *const RECT) as isize,
        );
        // SAFETY: valid out pointer.
        unsafe { GetWindowRect(host.hwnd(), &mut rect) }.unwrap();
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (1000, 800)
        );
        assert_eq!(host.info().dpi, 192);
        // A suggestion below the minimum size of the new DPI is raised to the minimum
        // (`WM_GETMINMAXINFO`): 480 x 320 DIPs are 960 x 640 pixels at 192 DPI.
        let tiny = RECT {
            left: -32000,
            top: -32000,
            right: -31800,
            bottom: -31900,
        };
        send(
            &host,
            WM_DPICHANGED,
            0x00C0_00C0,
            (&tiny as *const RECT) as isize,
        );
        // SAFETY: valid out pointer.
        unsafe { GetWindowRect(host.hwnd(), &mut rect) }.unwrap();
        let (min_width, min_height) = min_track_size_px(192);
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (min_width, min_height)
        );
    }

    #[test]
    fn os_version_check_uses_the_minimum_build() {
        let mut notified = 0;
        assert_eq!(
            check_os_version(
                OsVersion {
                    major: 10,
                    build: 22621
                },
                || notified += 1
            ),
            None
        );
        assert_eq!(
            check_os_version(
                OsVersion {
                    major: 11,
                    build: 1
                },
                || notified += 1
            ),
            None
        );
        assert_eq!(notified, 0);
        assert_eq!(
            check_os_version(
                OsVersion {
                    major: 10,
                    build: 22620
                },
                || notified += 1
            ),
            Some(EXIT_UNSUPPORTED_OS)
        );
        assert_eq!(
            check_os_version(
                OsVersion {
                    major: 6,
                    build: 9600
                },
                || notified += 1
            ),
            Some(EXIT_UNSUPPORTED_OS)
        );
        assert_eq!(notified, 2);
        let current = current_os_version();
        assert_eq!(current.major, 10);
        assert!(
            current.build >= MIN_OS_BUILD,
            "tests need Windows 11: {current:?}"
        );
    }

    #[test]
    fn hit_regions_map_to_codes_and_regions() {
        let codes = [
            (HitRegion::Client, HTCLIENT),
            (HitRegion::Caption, HTCAPTION),
            (HitRegion::Minimize, HTMINBUTTON),
            (HitRegion::Maximize, HTMAXBUTTON),
            (HitRegion::Close, HTCLOSE),
            (HitRegion::Resize(ResizeEdge::Top), HTTOP),
            (HitRegion::Resize(ResizeEdge::Bottom), HTBOTTOM),
            (HitRegion::Resize(ResizeEdge::Left), HTLEFT),
            (HitRegion::Resize(ResizeEdge::Right), HTRIGHT),
            (HitRegion::Resize(ResizeEdge::TopLeft), HTTOPLEFT),
            (HitRegion::Resize(ResizeEdge::TopRight), HTTOPRIGHT),
            (HitRegion::Resize(ResizeEdge::BottomLeft), HTBOTTOMLEFT),
            (HitRegion::Resize(ResizeEdge::BottomRight), HTBOTTOMRIGHT),
        ];
        for (region, code) in codes {
            assert_eq!(region.hit_code(), code, "{region:?}");
        }
        assert_eq!(button_for_hit(HTCLOSE), Some(CaptionButton::Close));
        assert_eq!(button_for_hit(HTMAXBUTTON), Some(CaptionButton::Maximize));
        assert_eq!(button_for_hit(HTMINBUTTON), Some(CaptionButton::Minimize));
        assert_eq!(button_for_hit(HTCAPTION), None);
        assert_eq!(
            top_resize_region(Point::new(5.0, 2.0), 800.0, 8.0),
            Some(HitRegion::Resize(ResizeEdge::TopLeft))
        );
        assert_eq!(
            top_resize_region(Point::new(795.0, 2.0), 800.0, 8.0),
            Some(HitRegion::Resize(ResizeEdge::TopRight))
        );
        assert_eq!(
            top_resize_region(Point::new(400.0, 2.0), 800.0, 8.0),
            Some(HitRegion::Resize(ResizeEdge::Top))
        );
        assert_eq!(top_resize_region(Point::new(400.0, 9.0), 800.0, 8.0), None);
        assert_eq!(
            default_hit_region(Point::new(100.0, 40.0), 800.0),
            HitRegion::Client
        );
        assert_eq!(
            default_hit_region(Point::new(100.0, 10.0), 800.0),
            HitRegion::Caption
        );
        assert_eq!(
            default_hit_region(Point::new(100.0, -1.0), 800.0),
            HitRegion::Client
        );
        assert_eq!(
            default_hit_region(Point::new(790.0, 10.0), 800.0),
            HitRegion::Close
        );
        assert_eq!(
            default_hit_region(Point::new(740.0, 10.0), 800.0),
            HitRegion::Maximize
        );
        assert_eq!(
            default_hit_region(Point::new(700.0, 10.0), 800.0),
            HitRegion::Minimize
        );
    }

    #[test]
    fn translation_helpers() {
        // Click counting.
        let mut tracker = ClickTracker::new(Duration::from_millis(500), 4.0);
        let at = Point::new(10.0, 10.0);
        let ms = Duration::from_millis;
        assert_eq!(tracker.press(ms(0), at, PointerButton::Left), 1);
        assert_eq!(
            tracker.press(ms(100), Point::new(12.0, 9.0), PointerButton::Left),
            2
        );
        assert_eq!(tracker.press(ms(200), at, PointerButton::Left), 3);
        assert_eq!(
            tracker.press(ms(300), at, PointerButton::Left),
            1,
            "the fourth click starts over"
        );
        assert_eq!(
            tracker.press(ms(350), at, PointerButton::Right),
            1,
            "another button"
        );
        assert_eq!(
            tracker.press(ms(400), Point::new(30.0, 10.0), PointerButton::Right),
            1,
            "moved too far"
        );
        assert_eq!(
            tracker.press(ms(1400), Point::new(30.0, 10.0), PointerButton::Right),
            1,
            "too slow"
        );
        // Modifiers.
        assert_eq!(modifiers_from(|_| false), Modifiers::NONE);
        let held = modifiers_from(|key| matches!(key, 0x10 | 0x11 | 0x12 | 0x5C));
        assert!(held.shift() && held.ctrl() && held.alt() && held.win());
        assert!(modifiers_from(|key| key == 0x5B).win());
        // Keys.
        let down = key_event(0x41, 0x0030_0001, true, Modifiers::CTRL);
        assert_eq!(
            (down.vk, down.scan, down.repeat, down.is_down),
            (0x41, 0x30, false, true)
        );
        let repeat = key_event(0x25, 0x4150_0001, true, Modifiers::NONE);
        assert_eq!((repeat.scan, repeat.repeat), (0xE050, true));
        let up = key_event(0x41, 0xC030_0001u32 as isize, false, Modifiers::NONE);
        assert!(!up.repeat && !up.is_down);
        // Text.
        let mut pending = None;
        assert_eq!(
            text_from_char(u16::from(b'a'), &mut pending).as_deref(),
            Some("a")
        );
        assert_eq!(text_from_char(0x0D, &mut pending), None);
        assert_eq!(text_from_char(0x08, &mut pending), None);
        assert_eq!(text_from_char(0xD83D, &mut pending), None);
        assert_eq!(text_from_char(0xDE00, &mut pending).as_deref(), Some("😀"));
        assert_eq!(
            text_from_char(0xDE00, &mut pending),
            None,
            "lone low surrogate"
        );
        assert_eq!(text_from_char(0xD83D, &mut pending), None);
        assert_eq!(
            text_from_char(u16::from(b'x'), &mut pending).as_deref(),
            Some("x")
        );
        assert!(pending.is_none());
        // Wheel.
        let at = Point::new(1.0, 2.0);
        let wheel = wheel_event(-30, false, at, Modifiers::CTRL);
        assert_eq!(
            (wheel.delta, wheel.precise, wheel.position),
            (Point::new(0.0, -0.25), false, at)
        );
        assert_eq!(
            wheel_event(240, true, at, Modifiers::NONE).delta,
            Point::new(2.0, 0.0)
        );
    }

    #[test]
    fn message_windows_and_helpers() {
        let owner = MessageWindow::new().unwrap();
        assert!(!owner.hwnd().is_invalid());
        assert!(wheel_scroll_lines() >= 1);
        let info = WindowInfo {
            hwnd: owner.hwnd(),
            dpi: 144,
            size_px: (300, 150),
            maximized: false,
            active: true,
        };
        assert_eq!(info.size_dip(), Size::new(200.0, 100.0));
        assert_eq!(dip_len_px(&info, 10.0), 15);
        assert_eq!(cursor_resource(Cursor::IBeam), IDC_IBEAM);
        for cursor in [
            Cursor::Arrow,
            Cursor::Hand,
            Cursor::Cross,
            Cursor::SizeWE,
            Cursor::SizeNS,
            Cursor::SizeNWSE,
            Cursor::SizeNESW,
            Cursor::SizeAll,
            Cursor::NotAllowed,
            Cursor::Wait,
            Cursor::Help,
        ] {
            assert!(!cursor_resource(cursor).is_null());
        }
        let _ = DrawOp::Save;
    }
}
