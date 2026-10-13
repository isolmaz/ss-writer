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
use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
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
    RegisterClassExW, SHOW_WINDOW_CMD, SIZE_MINIMIZED, SM_CXPADDEDBORDER, SM_CYSIZEFRAME,
    SPI_GETWHEELSCROLLLINES, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW, SW_SHOWMAXIMIZED,
    SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetCursor, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, WA_INACTIVE,
    WINDOW_EX_STYLE, WM_ACTIVATE, WM_APP, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCCREATE, WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDOWN,
    WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_PAINT, WM_POINTERDOWN, WM_POINTERLEAVE,
    WM_POINTERUP, WM_POINTERUPDATE, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR,
    WM_SETFOCUS, WM_SETTINGCHANGE, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASS_STYLES, WNDCLASSEXW, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_OVERLAPPEDWINDOW,
};
use windows::Win32::UI::WindowsAndMessaging::{POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, PT_TOUCHPAD};
use windows::core::{HSTRING, PCWSTR, w};

use crate::canvas::{CanvasAssets, D2dCanvas, TargetResources};
use crate::chrome::{self, CaptionButton, ChromeState, paint_caption_buttons};
use crate::com::{OsResultExt, WinError};
use crate::dpi::{
    DEFAULT_DPI, dip_to_px, dpi_from_wparam, min_track_size_px, point_px_to_dip, scale_for_dpi,
    size_px_to_dip, top_resize_band_px, window_dpi,
};
use crate::fault::check;
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
    OsVersion {
        major: 10,
        build: registry_build(w!("CurrentBuildNumber")),
    }
}

/// The number the text value `name` of the Windows version key holds; 0 when the value is
/// missing, too long for a build number or no number.
fn registry_build(name: PCWSTR) -> u32 {
    let mut buffer = [0u16; 16];
    let mut size = size_of_val(&buffer) as u32;
    // SAFETY: valid key path and a writable buffer of `size` bytes; read-only access.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            name,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            Some(&mut size),
        )
    };
    if status.is_ok() {
        crate::com::from_wide(&buffer).trim().parse().unwrap_or(0)
    } else {
        0
    }
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

/// How a window that was just created goes on screen ([`WindowHost::show_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowMode {
    /// It stays hidden (the windows of a test run).
    Hidden,
    /// It is shown without taking the focus (`--perf-offscreen`).
    NoActivate,
    /// It is shown maximized and activated (the window was maximized when it was stored).
    Maximized,
    /// It is shown at its normal size and activated.
    Normal,
}

impl ShowMode {
    /// The mode of a new window: a hidden application shows nothing, an off-screen window must
    /// not take the focus, and otherwise the stored `maximized` state decides.
    #[must_use]
    pub fn for_new_window(hidden: bool, offscreen: bool, maximized: bool) -> Self {
        if hidden {
            Self::Hidden
        } else if offscreen {
            Self::NoActivate
        } else if maximized {
            Self::Maximized
        } else {
            Self::Normal
        }
    }

    /// The `ShowWindow` command of the mode; `None` for a window that stays hidden.
    #[must_use]
    pub fn command(self) -> Option<SHOW_WINDOW_CMD> {
        match self {
            Self::Hidden => None,
            Self::NoActivate => Some(SW_SHOWNOACTIVATE),
            Self::Maximized => Some(SW_SHOWMAXIMIZED),
            Self::Normal => Some(SW_SHOW),
        }
    }
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
    // A high surrogate and a low one always make one scalar value, and a unit that is no
    // surrogate is one by itself: the lossless conversion never replaces anything.
    let text = if (0xDC00..0xE000).contains(&unit) {
        let high = pending_high.take()?;
        String::from_utf16_lossy(&[high, unit])
    } else {
        *pending_high = None;
        String::from_utf16_lossy(&[unit])
    };
    (!text.chars().any(char::is_control)).then_some(text)
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

/// Whether this process registered the window classes (they stay registered).
static REGISTERED: AtomicBool = AtomicBool::new(false);

fn register_classes() -> Result<(), WinError> {
    if REGISTERED.load(Ordering::Acquire) {
        return Ok(());
    }
    register_all().inspect(|()| REGISTERED.store(true, Ordering::Release))
}

/// Registers the window classes of this module (again, when a registration is not known).
fn register_all() -> Result<(), WinError> {
    register_class(CLASS_NAME, wndproc, CS_DBLCLKS)?;
    register_class(MESSAGE_CLASS_NAME, message_proc, WNDCLASS_STYLES(0))
}

/// Makes the next window creation register the classes again (tests: the registration is
/// process-wide, whatever other tests of the process did before).
#[cfg(all(test, feature = "fault-injection"))]
fn forget_registered_classes() {
    REGISTERED.store(false, Ordering::Release);
}

/// Registers the window class `name`; a class this process registered before counts as done.
fn register_class(
    name: PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    style: WNDCLASS_STYLES,
) -> Result<(), WinError> {
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
    let refused = atom == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS;
    let outcome = if refused {
        Err(windows::core::Error::from_thread())
    } else {
        Ok(())
    };
    check(outcome).ctx("RegisterClassExW")
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

/// Posts a message to the queue of one of this process's own windows (the performance scenarios
/// type into their own window this way: no synthetic input reaches the system). Returns whether
/// the message was queued.
pub fn post_message(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> bool {
    // SAFETY: a plain post of values the receiving procedure interprets itself (callers pass
    // no pointers); posting to a stale handle merely fails.
    unsafe { PostMessageW(Some(hwnd), msg, WPARAM(wparam), LPARAM(lparam)) }.is_ok()
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
        let created = unsafe {
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
        };
        // The window belongs to the value from here on: a failure injected after the call
        // destroys it again.
        check(created.map(|hwnd| Self { hwnd })).ctx("create message window")
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

/// What depends on the graphics devices: gone with them, rebuilt after a device loss.
struct Graphics {
    devices: GfxDevices,
    surface: WindowSurface,
    resources: TargetResources,
}

struct WindowState {
    hwnd: HWND,
    handler: Box<dyn WindowHandler>,
    dpi: u32,
    size_px: (i32, i32),
    chrome: ChromeState,
    theme: Theme,
    /// Images and scenes the content draws from: the window's own from `set_assets` on, so
    /// registrations survive until (and across) the graphics devices.
    assets: Option<CanvasAssets>,
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
#[derive(Debug, PartialEq)]
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

    /// Shows the cursor the widgets asked for over the client area.
    fn apply_cursor(&self) {
        // SAFETY: loads and sets a stock cursor; one that cannot be loaded (the stock
        // resources always load) leaves the cursor as it is.
        unsafe {
            let _ = LoadCursorW(None, cursor_resource(self.cursor))
                .map(|cursor| SetCursor(Some(cursor)));
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
        // Without the devices, or before the assets are set, there is nothing to draw with.
        let (Some(graphics), Some(assets)) = (self.graphics.as_mut(), self.assets.as_mut()) else {
            return Ok(());
        };
        let Graphics {
            surface, resources, ..
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
                // The class has no CS_HREDRAW / CS_VREDRAW, so the answer is always 0 (no
                // valid-rectangle flags): there is nothing to pass on.
                // SAFETY: default processing computes the standard frame insets.
                let _ = unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
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
                    params.rgrc[0].bottom -= autohide_inset(state);
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
                    self.apply_cursor();
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
                self.pointer_message(msg, query_pointer(id))
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
                crate::perf::input_received();
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
                self.assets = None;
                (Some(LRESULT(0)), After::None)
            }
            _ => (None, After::None),
        }
    }
}

/// What the system knows of the pointer `id` of a `WM_POINTER*` message: its device type and
/// state; `None` for a pointer it does not know (the message is stale or made up).
fn query_pointer(id: u32) -> Option<(POINTER_INPUT_TYPE, POINTER_INFO)> {
    let mut kind = POINTER_INPUT_TYPE(0);
    let mut info = POINTER_INFO::default();
    // SAFETY: valid out pointers. Both queries always run (and both fail for an unknown id),
    // so the answer has no branch only a real pointer could take.
    let (typed, informed) =
        unsafe { (GetPointerType(id, &mut kind), GetPointerInfo(id, &mut info)) };
    (typed.is_ok() & informed.is_ok()).then_some((kind, info))
}

impl WindowState {
    /// A `WM_POINTER*` message of the pointer `sample` describes. Only touch, pen and touchpad
    /// contacts become pointer events (the mouse arrives as `WM_MOUSE*` messages); anything else,
    /// and a pointer the system does not know, is left to default processing.
    fn pointer_message(
        &mut self,
        msg: u32,
        sample: Option<(POINTER_INPUT_TYPE, POINTER_INFO)>,
    ) -> (Option<LRESULT>, After) {
        let Some((kind, info)) = sample else {
            return (None, After::None);
        };
        let pointer_type = match kind {
            PT_TOUCH => PointerType::Touch,
            PT_PEN => PointerType::Pen,
            PT_TOUCHPAD => PointerType::Touchpad,
            _ => return (None, After::None),
        };
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
}

/// Barrel / second-button flag of `POINTER_INFO::pointerFlags`.
const POINTER_FLAG_SECONDBUTTON: u32 = 0x0000_0020;

/// How many pixels a maximized window leaves free at the bottom of the monitor: one when the
/// taskbar (`appbar_state` of `ABM_GETSTATE`) hides itself, so that it can still be reached.
fn autohide_inset(appbar_state: u32) -> i32 {
    i32::from(appbar_state & ABS_AUTOHIDE != 0)
}

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
            // Nothing holds the state while the window is created.
            cell.borrow_mut().hwnd = hwnd;
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
            assets: None,
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
        let created = unsafe {
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
        };
        // From here on `host` owns the window: every early return below destroys it (also a
        // failure injected right after the call), so the window procedure can never meet the
        // state pointer after the `Rc` is gone.
        let host = check(created.map(|hwnd| Self { hwnd, state })).ctx("CreateWindowExW")?;
        let hwnd = host.hwnd;
        crate::perf::mark(crate::perf::MARK_WINDOW_CREATED);
        let dpi = window_dpi(hwnd);
        let mut client = RECT::default();
        // SAFETY: valid out pointer.
        check(unsafe { GetClientRect(hwnd, &mut client) }).ctx("GetClientRect")?;
        {
            let mut borrowed = host.state.borrow_mut();
            borrowed.hwnd = hwnd;
            borrowed.dpi = dpi;
            borrowed.size_px = (client.right - client.left, client.bottom - client.top);
        }
        apply_dark_mode(hwnd, options.dark);
        set_dwm_i32(hwnd, DWMWA_SYSTEMBACKDROP_TYPE.0, DWMSBT_TABBEDWINDOW.0);
        crate::perf::mark(crate::perf::MARK_BACKDROP_APPLIED);
        set_dwm_i32(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE.0, DWMWCP_ROUND.0);
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        // SAFETY: valid window and margins; re-running NCCALCSIZE applies the custom frame.
        unsafe {
            check(DwmExtendFrameIntoClientArea(hwnd, &margins))
                .ctx("DwmExtendFrameIntoClientArea")?;
            check(SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            ))
            .ctx("SetWindowPos")?;
        }
        let mut client = RECT::default();
        // SAFETY: valid out pointer.
        check(unsafe { GetClientRect(hwnd, &mut client) }).ctx("GetClientRect")?;
        host.state.borrow_mut().size_px = (client.right - client.left, client.bottom - client.top);
        Ok(host)
    }

    /// Gives the window the canvas assets its content draws from (replacing earlier ones).
    /// They need no graphics devices, so a window can take images and scenes before
    /// [`WindowHost::attach_graphics`] and keeps them across a device loss.
    pub fn set_assets(&self, assets: CanvasAssets) {
        self.state.borrow_mut().assets = Some(assets);
    }

    /// Creates the swap chain and D2D resources on `devices`; the window renders from now on
    /// (once it has assets, see [`WindowHost::set_assets`]). It is not repainted here: the
    /// caller of a window that is already showing asks with [`WindowHost::invalidate`].
    ///
    /// # Errors
    /// Fails when the composition surface cannot be created.
    pub fn attach_graphics(&self, devices: GfxDevices) -> Result<(), WinError> {
        let mut state = self.state.borrow_mut();
        let (width, height) = (state.size_px.0.max(1) as u32, state.size_px.1.max(1) as u32);
        let surface = WindowSurface::new(&devices, self.hwnd, width, height, state.dpi as f32)?;
        let resources = TargetResources::new(devices.d2d_factory().clone(), devices.generation());
        state.graphics = Some(Graphics {
            devices,
            surface,
            resources,
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
    ///
    /// Windows asks for the cursor (`WM_SETCURSOR`) *before* it delivers the move that decides
    /// it, so a shape chosen while handling that move has to be set here as well; waiting for
    /// the next `WM_SETCURSOR` leaves the previous surface's shape on screen until the pointer
    /// moves again (a pointer that jumps, or a window that opens below a resting pointer,
    /// never moves).
    pub fn set_cursor(&self, cursor: Cursor) {
        let mut state = self.state.borrow_mut();
        if state.cursor == cursor {
            return;
        }
        state.cursor = cursor;
        if state.tracking_client {
            state.apply_cursor();
        }
    }

    /// Schedules a repaint.
    pub fn invalidate(&self) {
        self.state.borrow().invalidate();
    }

    /// Mutable access to the canvas assets (image and scene registration); `None` before
    /// [`WindowHost::set_assets`].
    pub fn with_assets<R>(&self, f: impl FnOnce(&mut CanvasAssets) -> R) -> Option<R> {
        let mut state = self.state.borrow_mut();
        state.assets.as_mut().map(f)
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

    /// Puts the window on screen the way `mode` says (nothing happens for
    /// [`ShowMode::Hidden`]).
    pub fn show_mode(&self, mode: ShowMode) {
        let Some(command) = mode.command() else {
            return;
        };
        // SAFETY: shows our own window.
        unsafe {
            let _ = ShowWindow(self.hwnd, command);
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
        self.show_mode(ShowMode::Normal);
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
    // A failing call leaves `lines` at its default of 3.
    // SAFETY: `lines` is a valid u32 out buffer.
    let _ = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&mut lines as *mut u32).cast()),
            windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    lines
}

/// Pixel size of a DIP length at the window's DPI.
#[must_use]
pub fn dip_len_px(info: &WindowInfo, dip: f32) -> i32 {
    dip_to_px(dip, info.scale())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::to_wide;
    use ssw_ui::canvas::DrawOp;
    use ssw_ui::geometry::Rect;
    use ssw_ui::theme::DEFAULT_ACCENT;
    use std::cell::RefCell as Cell;
    use windows::Win32::Graphics::Gdi::ClientToScreen;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursor, GetWindowRect, SendMessageW};

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
            assert_eq!(after, After::None);
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
        let key_of = |event: &Event| match event {
            Event::Key(key) => Some((key.vk, key.scan, key.is_down, key.repeat)),
            _ => None,
        };
        assert_eq!(key_of(&events[0]), Some((0x0D, 0x1C, true, false)));
        assert_eq!(key_of(&events[1]), Some((0x0D, 0x1C, false, false)));
        assert_eq!(key_of(&events[2]), None, "text is no key");
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
        assert_eq!(after, After::None);
        assert!(!host.info().active);
        feed(&host, WM_ACTIVATE, 1, 0);
        assert!(host.info().active);
        feed(&host, WM_ACTIVATE, usize::from(WA_INACTIVE as u16), 0);
        send(&host, WM_NCACTIVATE, 1, 0);
        send(&host, WM_NCACTIVATE, 0, 0);
        // The same state again changes nothing.
        send(&host, WM_NCACTIVATE, 0, 0);
        assert!(!host.info().active);
        assert_eq!(autohide_inset(0), 0, "a taskbar that stays needs no room");
        assert_eq!(
            autohide_inset(ABS_AUTOHIDE),
            1,
            "a hiding taskbar keeps a pixel"
        );
        assert_eq!(autohide_inset(ABS_AUTOHIDE | 2), 1);
        // A pointer message for a pointer that does not exist is left to default processing.
        let (result, after) = feed(&host, WM_POINTERDOWN, 0xFFFE, 0);
        assert_eq!(result, None);
        assert_eq!(after, After::None);
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
    fn a_new_cursor_is_shown_at_once_while_the_pointer_is_over_the_client_area() {
        // SAFETY: reads the cursor of this thread; the stock cursors always load.
        let shown = || unsafe { GetCursor() };
        let stock = |resource| {
            // SAFETY: loads a stock cursor.
            unsafe { LoadCursorW(None, resource) }.unwrap()
        };
        let (host, _log) = host(true, false);

        // Without the pointer over the window the shape is only remembered: Windows asks for
        // it when the pointer arrives.
        host.set_cursor(Cursor::Hand);
        assert_eq!(host.state.borrow().cursor, Cursor::Hand);
        send(&host, WM_SETCURSOR, 0, HTCLIENT as isize);
        assert_eq!(shown(), stock(IDC_HAND));

        // A move brings the pointer in; `WM_SETCURSOR` has already been answered for it, so a
        // shape decided while handling the move has to reach the screen without another move.
        send(&host, WM_MOUSEMOVE, 0, (8 << 16) | 4);
        assert!(host.state.borrow().tracking_client);
        host.set_cursor(Cursor::IBeam);
        assert_eq!(shown(), stock(IDC_IBEAM), "the new shape is shown at once");

        // The same shape again is not set again, and after the pointer left the window only
        // remembers.
        host.set_cursor(Cursor::IBeam);
        assert_eq!(shown(), stock(IDC_IBEAM));
        send(&host, WM_MOUSELEAVE, 0, 0);
        assert!(!host.state.borrow().tracking_client);
        host.set_cursor(Cursor::Arrow);
        assert_eq!(
            shown(),
            stock(IDC_IBEAM),
            "the shape of another window stays"
        );
        assert_eq!(host.state.borrow().cursor, Cursor::Arrow);
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
        // The loop raises a parked panic after the message that caused it: the wake-up is
        // posted when the loop is idle for the first time.
        let wake = host.wake_handle();
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_loop(|| {
                assert!(wake.wake());
                Some(Duration::from_millis(50))
            })
        }));
        assert_eq!(payload_text(&*outcome.unwrap_err()), Some("wake 3"));
        // The handlers of this test that ignore events and painting are told some, too.
        let info = host.info();
        let mut bomb = Bomb {
            wakes: 0,
            fragile_destroy: false,
        };
        bomb.on_event(&info, Event::Timer(ssw_ui::services::TimerId::new(1)));
        let mut canvas = ssw_ui::canvas::RecordingCanvas::default();
        bomb.on_paint(&info, &mut canvas);
        assert!(canvas.recorded().is_empty());
        let mut answering = Answering;
        answering.on_event(&info, Event::Timer(ssw_ui::services::TimerId::new(1)));
        answering.on_paint(&info, &mut canvas);
        assert!(canvas.recorded().is_empty());
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
    fn graphics_attach_renders_and_resizes() {
        // COM is initialized first so it is torn down last, after the window released its
        // D2D / WIC objects.
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let (host, log) = host(true, false);
        let devices = crate::gfx::DeviceStartup::spawn().wait().unwrap();
        let wic = crate::wic::Wic::new().unwrap();
        let engine = crate::dwrite::DWriteEngine::new().unwrap();
        let assets = CanvasAssets::new(crate::canvas::ImageStore::new(wic), engine);
        // The assets belong to the window and need no devices; nothing is painted without them.
        host.set_assets(assets);
        assert_eq!(host.graphics_generation(), None);
        assert_eq!(host.with_assets(|assets| assets.images.len()), Some(0));
        send(&host, WM_PAINT, 0, 0);
        assert_eq!(log.borrow().paints, 0, "no devices, no frame");
        host.attach_graphics(devices).unwrap();
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
    fn a_frame_needs_the_devices_and_the_assets_and_destroying_the_window_releases_both() {
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let (host, log) = host(true, false);
        let devices = crate::gfx::GfxDevices::create_on(crate::gfx::Driver::Warp, 1).unwrap();
        host.attach_graphics(devices).unwrap();
        assert_eq!(host.with_assets(|_| ()), None);
        send(&host, WM_PAINT, 0, 0);
        assert_eq!(log.borrow().paints, 0, "devices alone have nothing to draw");
        let wic = crate::wic::Wic::new().unwrap();
        let engine = crate::dwrite::DWriteEngine::new().unwrap();
        host.set_assets(CanvasAssets::new(
            crate::canvas::ImageStore::new(wic),
            engine,
        ));
        send(&host, WM_PAINT, 0, 0);
        assert_eq!(log.borrow().paints, 1);
        assert_eq!(host.graphics_generation(), Some(1));
        host.destroy();
        assert_eq!(host.graphics_generation(), None);
        assert_eq!(host.with_assets(|_| ()), None);
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
        assert_eq!(registry_build(w!("CurrentBuildNumber")), current.build);
        assert_eq!(
            registry_build(w!("NoSuchBuildValue")),
            0,
            "a missing value is build 0"
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
        // Modifiers: `modifiers_from` is generic over the key query, so every combination goes
        // through one closure type (one instantiation sees each key down and up).
        let pressed = |keys: &[u16]| modifiers_from(|key| keys.contains(&key));
        assert_eq!(pressed(&[]), Modifiers::NONE);
        let held = pressed(&[0x10, 0x11, 0x12, 0x5C]);
        assert!(held.shift() && held.ctrl() && held.alt() && held.win());
        let left_windows = pressed(&[0x5B]);
        assert!(left_windows.win() && !left_windows.shift());
        assert_eq!(pressed(&[0x11]), Modifiers::CTRL);
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

    #[test]
    fn a_registered_timer_ticks_the_handler_and_a_one_shot_timer_is_forgotten() {
        use ssw_ui::services::TimerId;
        use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer};

        let (host, log) = host(true, false);
        let hwnd = host.hwnd();
        let timers = Rc::clone(&host.state.borrow().timers);
        let repeating = timers.borrow_mut().register(TimerId::new(900), true);
        let once = timers.borrow_mut().register(TimerId::new(901), false);
        // SAFETY: real OS timers of our own window with the ids of the table; the window kills
        // the one-shot, the test the other.
        unsafe {
            let _ = SetTimer(Some(hwnd), repeating, 60_000, None);
            let _ = SetTimer(Some(hwnd), once, 60_000, None);
        }
        let before = log.borrow().events.len();
        assert_eq!(send(&host, WM_TIMER, repeating, 0), 0);
        assert_eq!(send(&host, WM_TIMER, once, 0), 0);
        assert_eq!(
            log.borrow().events[before..],
            [
                Event::Timer(TimerId::new(900)),
                Event::Timer(TimerId::new(901))
            ]
        );
        assert_eq!(timers.borrow().len(), 1, "a one-shot timer is forgotten");
        // SAFETY: kills timers of our own window; the one-shot is already gone.
        unsafe {
            assert!(KillTimer(Some(hwnd), once).is_err(), "the window killed it");
            assert!(KillTimer(Some(hwnd), repeating).is_ok());
        }
    }

    #[test]
    fn contacts_become_pointer_events_and_the_mouse_and_unknown_pointers_are_left_alone() {
        use windows::Win32::UI::Input::Pointer::POINTER_FLAGS;
        use windows::Win32::UI::WindowsAndMessaging::PT_MOUSE;

        let (host, log) = host(true, false);
        let message = |msg: u32, kind: POINTER_INPUT_TYPE, flags: u32| {
            let info = POINTER_INFO {
                ptPixelLocation: POINT { x: 30, y: 40 },
                pointerFlags: POINTER_FLAGS(flags),
                ..Default::default()
            };
            let (result, after) = host
                .state
                .borrow_mut()
                .pointer_message(msg, Some((kind, info)));
            (result.map(|result| result.0), after)
        };
        let before = log.borrow().events.len();
        // The mouse has its own messages; a pointer the system does not know has no type.
        assert_eq!(message(WM_POINTERDOWN, PT_MOUSE, 0), (None, After::None));
        let unknown = host
            .state
            .borrow_mut()
            .pointer_message(WM_POINTERDOWN, None);
        assert_eq!((unknown.0, unknown.1), (None, After::None));
        assert_eq!(log.borrow().events.len(), before, "no event was made");
        // A focus event shows that the filter below skips what is no pointer event.
        feed(&host, WM_SETFOCUS, 0, 0);
        for (msg, kind, flags) in [
            (WM_POINTERDOWN, PT_TOUCH, 0),
            (WM_POINTERUPDATE, PT_TOUCH, 0),
            (WM_POINTERUP, PT_TOUCH, 0),
            (WM_POINTERLEAVE, PT_PEN, 0),
            (WM_POINTERDOWN, PT_PEN, POINTER_FLAG_SECONDBUTTON),
            (WM_POINTERDOWN, PT_TOUCHPAD, 0),
        ] {
            assert_eq!(message(msg, kind, flags), (Some(0), After::None));
        }
        let seen: Vec<_> = log.borrow().events[before..]
            .iter()
            .filter_map(|event| match event {
                Event::Pointer(p) => Some((p.kind, p.button, p.pointer_type)),
                _ => None,
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    PointerType::Touch
                ),
                (PointerEventKind::Move, None, PointerType::Touch),
                (
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    PointerType::Touch
                ),
                (PointerEventKind::Leave, None, PointerType::Pen),
                (
                    PointerEventKind::Down,
                    Some(PointerButton::Right),
                    PointerType::Pen
                ),
                (
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    PointerType::Touchpad
                ),
            ]
        );
    }

    #[test]
    fn the_caption_buttons_maximize_restore_and_minimize_a_window_of_a_private_desktop() {
        use crate::testing::PrivateDesktop;
        use windows::Win32::UI::WindowsAndMessaging::IsIconic;

        fn zoomed(host: &WindowHost) -> bool {
            // SAFETY: a plain query of a window of this thread.
            unsafe { IsZoomed(host.hwnd()) }.as_bool()
        }

        fn iconic(host: &WindowHost) -> bool {
            // SAFETY: a plain query of a window of this thread.
            unsafe { IsIconic(host.hwnd()) }.as_bool()
        }

        // Windows are shown, maximized and minimized only on a desktop of their own.
        let desktop = PrivateDesktop::create().unwrap();
        desktop.run(|| {
            let (host, _log) = host(true, false);
            host.show_mode(ShowMode::NoActivate);
            let click = |code: u32| {
                send(&host, WM_NCLBUTTONDOWN, code as usize, 0);
                send(&host, WM_NCLBUTTONUP, code as usize, 0);
            };
            assert!(!zoomed(&host) && !iconic(&host));
            click(HTMAXBUTTON);
            assert!(zoomed(&host), "the maximize button maximizes");
            click(HTMAXBUTTON);
            assert!(!zoomed(&host), "and restores a maximized window");
            click(HTMINBUTTON);
            assert!(iconic(&host), "the minimize button minimizes");
        });
    }

    /// A handler that implements only what the trait requires.
    struct Minimal;

    impl WindowHandler for Minimal {
        fn on_event(&mut self, _info: &WindowInfo, _event: Event) {}
        fn on_paint(&mut self, _info: &WindowInfo, canvas: &mut dyn Canvas) {
            canvas.fill_rect(
                Rect::new(0.0, 0.0, 1.0, 1.0),
                ssw_ui::geometry::Color::BLACK,
            );
        }
    }

    #[test]
    fn the_provided_handler_methods_do_nothing_and_accept_the_default_answers() {
        let (host, _log) = host(true, false);
        let info = host.info();
        let mut handler = Minimal;
        let top_left = Point::new(300.0, 10.0);
        assert_eq!(
            handler.hit_test(top_left, &info),
            default_hit_region(top_left, info.size_dip().width)
        );
        handler.on_event(&info, Event::Timer(ssw_ui::services::TimerId::new(1)));
        let mut canvas = ssw_ui::canvas::RecordingCanvas::default();
        handler.on_paint(&info, &mut canvas);
        handler.on_resize(&info);
        handler.on_activate(&info, true);
        handler.on_theme_changed(&info, &read_system_theme(&Win32Settings));
        handler.on_wake(&info);
        assert!(
            handler.on_close_requested(&info),
            "closing is allowed by default"
        );
        handler.on_destroy(&info);
        assert_eq!(
            handler.on_message(&info, WM_APP, WPARAM(0), LPARAM(0)),
            None,
            "messages are not answered by default"
        );
    }

    /// A handler that answers one private message before the window translates anything.
    struct Answering;

    impl WindowHandler for Answering {
        fn on_event(&mut self, _info: &WindowInfo, _event: Event) {}
        fn on_paint(&mut self, _info: &WindowInfo, _canvas: &mut dyn Canvas) {}
        fn on_message(
            &mut self,
            _info: &WindowInfo,
            msg: u32,
            _wparam: WPARAM,
            _lparam: LPARAM,
        ) -> Option<LRESULT> {
            (msg == WM_APP + 7).then_some(LRESULT(7))
        }
    }

    #[test]
    fn a_handler_answer_to_a_message_ends_the_translation() {
        let options = WindowOptions {
            title: "answering".to_owned(),
            rect: Some((-32000, -32000, 800, 600)),
            dark: false,
            no_activate: true,
        };
        let host = WindowHost::create(&options, Box::new(Answering), Theme::light(DEFAULT_ACCENT))
            .unwrap();
        let (result, after) = feed(&host, WM_APP + 7, 0, 0);
        assert_eq!(result, Some(7));
        assert_eq!(after, After::None);
        // Another message goes on to the window's own translation: an unknown timer is ours.
        let (result, _) = feed(&host, WM_TIMER, 0xDEAD, 0);
        assert_eq!(result, Some(0));
        // The handler's own answer reaches the sender through the real window procedure.
        assert_eq!(send_message(host.hwnd(), WM_APP + 7, 0, 0), 7);
    }

    /// While it handles `WM_APP + 8`, sends `WM_APP + 7` (which it answers with 7) to its own
    /// window and keeps what came back.
    struct Reentrant(Rc<std::cell::Cell<Option<isize>>>);

    impl WindowHandler for Reentrant {
        fn on_event(&mut self, _info: &WindowInfo, _event: Event) {}
        fn on_paint(&mut self, _info: &WindowInfo, _canvas: &mut dyn Canvas) {}
        fn on_message(
            &mut self,
            info: &WindowInfo,
            msg: u32,
            _wparam: WPARAM,
            _lparam: LPARAM,
        ) -> Option<LRESULT> {
            match msg {
                m if m == WM_APP + 7 => Some(LRESULT(7)),
                m if m == WM_APP + 8 => {
                    self.0.set(Some(send_message(info.hwnd, WM_APP + 7, 0, 0)));
                    Some(LRESULT(8))
                }
                _ => None,
            }
        }
    }

    #[test]
    fn a_message_sent_while_the_handler_runs_takes_the_default_path() {
        let options = WindowOptions {
            title: "re-entrant".to_owned(),
            rect: Some((-32000, -32000, 800, 600)),
            dark: false,
            no_activate: true,
        };
        let nested = Rc::new(std::cell::Cell::new(None));
        let handler = Box::new(Reentrant(Rc::clone(&nested)));
        let host = WindowHost::create(&options, handler, Theme::light(DEFAULT_ACCENT)).unwrap();
        // Sent on its own, the message reaches the handler.
        assert_eq!(send_message(host.hwnd(), WM_APP + 7, 0, 0), 7);
        // Sent while the handler holds the window state, it does not: the default window
        // procedure answers it (0) instead of a second, aliasing borrow of the state.
        assert_eq!(send_message(host.hwnd(), WM_APP + 8, 0, 0), 8);
        assert_eq!(nested.get(), Some(0));
        // Like the other handlers of these tests, this one ignores events and painting; told
        // some, it leaves the canvas untouched.
        let info = host.info();
        let mut handler = Reentrant(nested);
        handler.on_event(&info, Event::Timer(ssw_ui::services::TimerId::new(1)));
        let mut canvas = ssw_ui::canvas::RecordingCanvas::default();
        handler.on_paint(&info, &mut canvas);
        assert!(canvas.recorded().is_empty());
    }

    #[test]
    fn a_wake_handle_made_from_a_window_wakes_it_until_it_is_gone() {
        let (host, log) = host(true, false);
        let wake = WakeHandle::new(host.hwnd());
        assert_eq!(wake, host.wake_handle());
        assert!(wake.wake());
        let mut message = MSG::default();
        // SAFETY: takes the posted wake-up message of our own window off this thread's queue.
        let found =
            unsafe { PeekMessageW(&mut message, Some(host.hwnd()), WM_WAKE, WM_WAKE, PM_REMOVE) };
        assert!(found.as_bool());
        assert_eq!(
            log.borrow().wakes,
            0,
            "the message was removed, not dispatched"
        );
        assert_eq!(send_message(host.hwnd(), WM_WAKE, 0, 0), 0);
        assert_eq!(log.borrow().wakes, 1);
    }

    #[test]
    fn caption_button_releases_choose_the_window_command() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_STYLE, GetWindowLongPtrW, HTCAPTION, HTCLOSE, HTMAXBUTTON, HTMINBUTTON, WS_MAXIMIZE,
        };

        let (host, _log) = host(true, false);
        let press = |hit: u32| feed(&host, WM_NCLBUTTONDOWN, hit as usize, 0);
        let release = |hit: u32| feed(&host, WM_NCLBUTTONUP, hit as usize, 0);

        // A press on the caption itself is left to the system.
        let (result, after) = press(HTCAPTION);
        assert_eq!(result, None);
        assert_eq!(after, After::None);

        press(HTMINBUTTON);
        let (result, after) = release(HTMINBUTTON);
        assert_eq!(result, Some(0));
        assert_eq!(after, After::Show(SW_MINIMIZE.0));

        press(HTMAXBUTTON);
        let (_, after) = release(HTMAXBUTTON);
        assert_eq!(after, After::Show(SW_MAXIMIZE.0));

        // Pressed on one button, released on another: swallowed, nothing happens.
        press(HTMINBUTTON);
        let (result, after) = release(HTCLOSE);
        assert_eq!(result, Some(0));
        assert_eq!(after, After::None);
        // Released without a press: the system sees it.
        let (result, after) = release(HTCLOSE);
        assert_eq!(result, None);
        assert_eq!(after, After::None);

        // A maximized window restores instead (the style bit is what `IsZoomed` reads; the
        // window is never shown).
        // SAFETY: plain style changes of our own hidden window, undone below.
        let style = unsafe { GetWindowLongPtrW(host.hwnd(), GWL_STYLE) };
        // SAFETY: sets the maximize bit on our own hidden window; restored below.
        unsafe { SetWindowLongPtrW(host.hwnd(), GWL_STYLE, style | WS_MAXIMIZE.0 as isize) };
        press(HTMAXBUTTON);
        let (_, after) = release(HTMAXBUTTON);
        // SAFETY: restores the style read above.
        unsafe { SetWindowLongPtrW(host.hwnd(), GWL_STYLE, style) };
        assert_eq!(after, After::Show(SW_RESTORE.0));
    }

    #[test]
    fn a_maximized_frame_gives_the_caption_back_and_keeps_the_monitor_overhang() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GWL_STYLE, GetWindowLongPtrW, WINDOWPOS, WS_MAXIMIZE,
        };

        let (host, _log) = host(true, false);
        // Without the parameter block the message is left to the system.
        let (result, after) = feed(&host, WM_NCCALCSIZE, 1, 0);
        assert_eq!(result, None);
        assert_eq!(after, After::None);

        let mut position = WINDOWPOS::default();
        let mut params = NCCALCSIZE_PARAMS::default();
        params.rgrc[0] = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        params.lppos = &mut position;
        // Restored: the client area starts at the top of the window (the caption is ours).
        let (result, _) = feed(&host, WM_NCCALCSIZE, 1, (&raw mut params) as isize);
        assert_eq!(result, Some(0));
        assert_eq!(params.rgrc[0].top, 0);

        // SAFETY: plain style changes of our own hidden window, undone below.
        let style = unsafe { GetWindowLongPtrW(host.hwnd(), GWL_STYLE) };
        // SAFETY: sets the maximize bit on our own hidden window; restored below.
        unsafe { SetWindowLongPtrW(host.hwnd(), GWL_STYLE, style | WS_MAXIMIZE.0 as isize) };
        params.rgrc[0] = RECT {
            left: 0,
            top: 0,
            right: 800,
            bottom: 600,
        };
        let (result, _) = feed(&host, WM_NCCALCSIZE, 1, (&raw mut params) as isize);
        // SAFETY: restores the style read above.
        unsafe { SetWindowLongPtrW(host.hwnd(), GWL_STYLE, style) };
        assert_eq!(result, Some(0));
        // A maximized window overhangs the monitor by its frame: the client area starts below
        // the overhang.
        let dpi = host.info().dpi;
        // SAFETY: pure metric queries.
        let overhang = unsafe {
            GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi)
                + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
        };
        assert!(overhang > 0);
        assert_eq!(params.rgrc[0].top, overhang);
    }

    #[test]
    fn a_window_that_is_gone_ignores_the_requests_to_show_it_and_has_no_placement() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        let (host, _log) = host(true, false);
        // A live (never shown) window answers the queries.
        let (_, _, width, height) = host.normal_placement().unwrap();
        assert_eq!((width, height), (800, 600));
        let origin = host.client_to_screen(0, 0);
        let moved = host.client_to_screen(10, 20);
        assert_eq!((moved.0 - origin.0, moved.1 - origin.1), (10, 20));
        assert!(format!("{host:?}").starts_with("WindowHost { hwnd: HWND("));

        // Once it is destroyed its handle is stale and the queries find no window. (The show
        // and raise requests are never tried on a stale handle: the system may already have
        // given that handle value to another program's window.)
        host.destroy();
        assert_eq!(host.normal_placement(), None);
        assert_eq!(host.client_to_screen(10, 20), (10, 20));
        // SAFETY: plain query.
        assert!(!unsafe { IsWindowVisible(host.hwnd()) }.as_bool());
    }

    #[test]
    fn classes_register_once_and_an_unnamed_one_is_refused() {
        register_class(CLASS_NAME, wndproc, CS_DBLCLKS).unwrap();
        // The same class again is the class already there.
        register_class(CLASS_NAME, wndproc, CS_DBLCLKS).unwrap();
        let refused = register_class(PCWSTR::null(), wndproc, CS_DBLCLKS).unwrap_err();
        assert!(
            refused.to_string().starts_with("RegisterClassExW failed"),
            "{refused}"
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_system_call_of_creating_windows_is_reported_under_its_own_name() {
        use crate::fault::{all_failed, failed_step, sweep};

        let steps = sweep(|| {
            forget_registered_classes();
            failed_step(MessageWindow::new())
        });
        assert_eq!(
            steps,
            all_failed(&[
                "RegisterClassExW",
                "RegisterClassExW",
                "create message window"
            ])
        );
        let steps = sweep(|| {
            forget_registered_classes();
            let options = WindowOptions {
                title: "sweep".to_owned(),
                rect: Some((-32000, -32000, 400, 300)),
                dark: false,
                no_activate: true,
            };
            failed_step(WindowHost::create(
                &options,
                Box::new(Minimal),
                Theme::light(DEFAULT_ACCENT),
            ))
        });
        assert_eq!(
            steps,
            all_failed(&[
                "RegisterClassExW",
                "RegisterClassExW",
                "CreateWindowExW",
                "GetClientRect",
                "DwmExtendFrameIntoClientArea",
                "SetWindowPos",
                "GetClientRect",
            ])
        );
    }

    /// Gives `host` fresh graphics (generation 1), releasing the ones it had.
    #[cfg(feature = "fault-injection")]
    fn attach(host: &WindowHost) {
        host.state.borrow_mut().graphics = None;
        let devices = crate::gfx::GfxDevices::create(1).unwrap();
        let wic = crate::wic::Wic::new().unwrap();
        let engine = crate::dwrite::DWriteEngine::new().unwrap();
        host.set_assets(CanvasAssets::new(
            crate::canvas::ImageStore::new(wic),
            engine,
        ));
        host.attach_graphics(devices).unwrap();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_surface_that_cannot_be_built_leaves_the_window_without_graphics() {
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let (host, _log) = host(true, false);
        let devices = crate::gfx::GfxDevices::create(1).unwrap();
        crate::fault::arm(0);
        let error = host.attach_graphics(devices).unwrap_err();
        crate::fault::disarm();
        assert!(
            error.to_string().starts_with("DXGI adapter failed"),
            "{error}"
        );
        assert_eq!(host.graphics_generation(), None);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_lost_device_is_replaced_and_the_frame_is_painted_again_on_the_new_one() {
        use windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;

        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let (host, log) = host(true, false);
        attach(&host);

        // A frame that fails with a lost device: new devices, and the frame is drawn again.
        let paints = log.borrow().paints;
        crate::fault::arm_with(0, DXGI_ERROR_DEVICE_REMOVED);
        host.state.borrow_mut().paint();
        crate::fault::disarm();
        assert_eq!(host.graphics_generation(), Some(2));
        assert_eq!(log.borrow().paints, paints + 1);

        // A swap chain that cannot be resized because the device is lost: same recovery,
        // and the new surface has the new size.
        crate::fault::arm_with(0, DXGI_ERROR_DEVICE_REMOVED);
        host.state.borrow_mut().resize(300, 200);
        crate::fault::disarm();
        assert_eq!(host.graphics_generation(), Some(3));
        assert_eq!(host.info().size_px, (300, 200));

        // Any other resize failure keeps the devices.
        crate::fault::arm(0);
        host.state.borrow_mut().resize(310, 210);
        crate::fault::disarm();
        assert_eq!(host.graphics_generation(), Some(3));

        // Recovery with no devices to be made leaves the window without graphics, and without
        // graphics there is nothing to recover.
        crate::fault::arm_from(0);
        host.state.borrow_mut().recover_graphics();
        crate::fault::disarm();
        assert_eq!(host.graphics_generation(), None);
        host.state.borrow_mut().recover_graphics();
        assert_eq!(host.graphics_generation(), None);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn recovery_ends_without_graphics_when_the_new_surface_cannot_be_built() {
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let (host, _log) = host(true, false);
        // One failing call per run: the first calls are the device creation (the software
        // fallback takes over), the rest build the surface (nothing takes over).
        let mut generations = Vec::new();
        for call in 0.. {
            attach(&host);
            crate::fault::arm(call);
            host.state.borrow_mut().recover_graphics();
            let hit = crate::fault::tripped();
            crate::fault::disarm();
            if !hit {
                break;
            }
            generations.push(host.graphics_generation());
        }
        let mut expected = vec![Some(2); 5];
        expected.extend([None; 10]);
        assert_eq!(generations, expected);
    }

    #[test]
    fn a_minimized_size_message_changes_nothing_and_a_missing_size_block_is_left_alone() {
        let (host, log) = host(true, false);
        let size = host.info().size_px;
        let resizes = log.borrow().resizes.len();
        let (result, after) = feed(&host, WM_SIZE, SIZE_MINIMIZED as usize, 0);
        assert_eq!(result, Some(0));
        assert_eq!(after, After::None);
        assert_eq!(host.info().size_px, size);
        assert_eq!(log.borrow().resizes.len(), resizes);
        let (result, after) = feed(&host, WM_GETMINMAXINFO, 0, 0);
        assert_eq!(result, Some(0));
        assert_eq!(after, After::None);
    }

    #[test]
    fn nothing_is_painted_while_painting_or_without_a_client_area() {
        let (host, _log) = host(true, false);
        host.state.borrow_mut().painting = true;
        host.state.borrow_mut().paint();
        assert!(
            host.state.borrow().painting,
            "a paint in progress owns the flag"
        );
        host.state.borrow_mut().painting = false;
        for size in [(0, 10), (10, 0)] {
            host.state.borrow_mut().size_px = size;
            host.state.borrow_mut().paint();
            assert!(!host.state.borrow().painting);
        }
    }

    #[test]
    fn system_keys_and_foreign_timers_are_left_alone_or_ignored() {
        let (host, log) = host(true, false);
        // System keys (Alt+F4, Alt+Space) keep their default behavior after the handler saw them.
        let before = log.borrow().events.len();
        for msg in [WM_SYSKEYDOWN, WM_SYSKEYUP] {
            let (result, after) = feed(&host, msg, 0x7C, 0);
            assert_eq!(result, None);
            assert_eq!(after, After::None);
        }
        assert_eq!(log.borrow().events.len(), before + 2);
        // A timer this window never set (a stale tick) is swallowed without an event.
        let (result, after) = feed(&host, WM_TIMER, 0xDEAD, 0);
        assert_eq!(result, Some(0));
        assert_eq!(after, After::None);
        assert_eq!(log.borrow().events.len(), before + 2);
    }
}
