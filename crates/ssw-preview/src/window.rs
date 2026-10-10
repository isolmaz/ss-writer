//! The child window of the preview host: a DirectComposition swap chain that shows a
//! [`PreviewContent`], mouse and keyboard translated to editor events, and the keys the
//! preview does not use forwarded to the host's `IPreviewHandlerFrame`.
//!
//! Nothing may unwind out of the window procedure (the OS frames above it cannot be unwound
//! through): a panic while handling a message is caught and the message gets default
//! processing. A message that arrives while another one is being handled (a callback that sends
//! messages back) takes the default path instead of aliasing the state.

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ssw_ui::geometry::Point;
use ssw_ui::input::{
    Cursor, Event, Modifiers, PointerButton, PointerEvent, PointerEventKind, PointerType,
};
use ssw_ui::widget::EventResult;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::canvas::{CanvasAssets, D2dCanvas, ImageStore, TargetResources};
use ssw_win::com::{OsResultExt, WinError};
use ssw_win::dpi::{DEFAULT_DPI, point_px_to_dip, scale_for_dpi, window_dpi};
use ssw_win::gfx::{GfxDevices, WindowSurface};
use ssw_win::services::clipboard::WindowsClipboard;
use ssw_win::services::timers::{TimerTable, WindowTimers};
use ssw_win::theme::SystemTheme;
use ssw_win::wic::{DecodedImage, Wic};
use ssw_win::window::{ClickTracker, key_event, modifiers_from, wheel_event, wheel_scroll_lines};
use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{InvalidateRect, ScreenToClient, ValidateRect};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetFocus, GetKeyState, ReleaseCapture, SetCapture, SetFocus, TME_LEAVE,
    TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::Shell::IPreviewHandlerFrame;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CS_DBLCLKS, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA,
    GetClientRect, GetMessagePos, GetMessageTime, GetWindowLongPtrW, HTCLIENT, IDC_ARROW,
    IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL, IDC_SIZENESW, IDC_SIZENS,
    IDC_SIZENWSE, IDC_SIZEWE, IDC_WAIT, KillTimer, LoadCursorW, MSG, RegisterClassExW,
    SIZE_MINIMIZED, SWP_NOACTIVATE, SWP_NOZORDER, SetCursor, SetParent, SetWindowLongPtrW,
    SetWindowPos, UnregisterClassW, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS,
    WM_SIZE, WM_SYSKEYDOWN, WM_TIMER, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_CHILD,
    WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_NOREDIRECTIONBITMAP, WS_VISIBLE,
};
use windows::core::{PCWSTR, w};

use crate::appearance::Visuals;
use crate::content::PreviewContent;
use crate::fault;
use crate::server::module_handle;

const CLASS_NAME: PCWSTR = w!("SSWriterPreviewWindow");

/// `WM_MOUSELEAVE`.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// `WM_DPICHANGED_AFTERPARENT`: the DPI of the window changed because its parent's did.
const WM_DPICHANGED_AFTERPARENT: u32 = 0x0739;
/// `WM_GETDLGCODE`: the window asks which keys it wants instead of a dialog manager.
const WM_GETDLGCODE: u32 = 0x0087;
/// `DLGC_WANTARROWS`: the arrow keys are the window's.
const DLGC_WANTARROWS: isize = 0x0001;

/// Pixel slop and double-click interval of the click counter.
const CLICK_SLOP: f32 = 4.0;

/// How many windows of this class are alive. The class is registered with the first and
/// unregistered with the last, so no registration outlives this module's code (the DLL can be
/// unloaded once every window is gone).
static CLASS_USERS: Mutex<usize> = Mutex::new(0);

/// The class user count; a count left behind by a panicking thread is still a valid count.
fn class_users() -> MutexGuard<'static, usize> {
    CLASS_USERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The window class of the preview child window.
fn window_class() -> WNDCLASSEXW {
    WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: module_handle(),
        // SAFETY: loading a stock cursor.
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    }
}

/// Registers `class`. A class that already exists is fine: it is the registration an earlier
/// [`release_class`] could not remove.
fn register_class(class: &WNDCLASSEXW) -> Result<(), WinError> {
    fault::point()?;
    // SAFETY: the class structure is fully initialized (the caller's contract) and outlives the
    // call.
    let atom = unsafe { RegisterClassExW(class) };
    // SAFETY: reads the calling thread's last error right after the failed call.
    if atom == 0 && unsafe { GetLastError() } != ERROR_CLASS_ALREADY_EXISTS {
        return Err(WinError::from_os(
            "RegisterClassExW",
            windows::core::Error::from_thread(),
        ));
    }
    Ok(())
}

/// Takes a use of the window class, registering it for the first user.
fn acquire_class() -> Result<(), WinError> {
    let mut users = class_users();
    if *users == 0 {
        register_class(&window_class())?;
    }
    *users += 1;
    Ok(())
}

/// Gives a use of the window class back, unregistering it for the last one.
fn release_class() {
    let mut users = class_users();
    *users = users.saturating_sub(1);
    if *users == 0 {
        // SAFETY: no window of the class is left; a failure leaves a registration that the
        // next `acquire_class` accepts as already existing.
        unsafe {
            let _ = UnregisterClassW(CLASS_NAME, Some(module_handle()));
        }
    }
}

/// The resources that draw into the window.
struct Graphics {
    devices: GfxDevices,
    surface: WindowSurface,
    resources: TargetResources,
    assets: CanvasAssets,
}

/// What a message handler asks for once the state is no longer borrowed (functions that send
/// messages back into the window procedure).
enum After {
    None,
    /// Give the keyboard focus to the window.
    Focus,
    /// Offer a key the preview does not use to the host.
    Translate(MSG),
}

struct WindowState {
    hwnd: HWND,
    content: PreviewContent,
    graphics: Option<Graphics>,
    frame: Option<IPreviewHandlerFrame>,
    dpi: u32,
    size_px: (i32, i32),
    clicks: ClickTracker,
    started: Instant,
    cursor: Cursor,
    tracking_mouse: bool,
    captured: bool,
    timers: Rc<RefCell<TimerTable>>,
    painting: bool,
}

/// The device-independent point of a packed `x | y << 16` message parameter.
fn point_of(lparam: LPARAM) -> POINT {
    POINT {
        x: i32::from((lparam.0 & 0xFFFF) as i16),
        y: i32::from(((lparam.0 >> 16) & 0xFFFF) as i16),
    }
}

fn current_modifiers() -> Modifiers {
    modifiers_from(|key| {
        // SAFETY: pure state query.
        unsafe { GetKeyState(i32::from(key)) < 0 }
    })
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

impl WindowState {
    fn scale(&self) -> f32 {
        scale_for_dpi(self.dpi)
    }

    fn client_point(&self, screen: POINT) -> Point {
        let mut point = screen;
        // SAFETY: converts using our own window handle; a failure leaves screen coordinates.
        let _ = unsafe { ScreenToClient(self.hwnd, &mut point) };
        point_px_to_dip(point.x, point.y, self.scale())
    }

    fn invalidate(&self) {
        // SAFETY: invalidates our own window; the repaint arrives as WM_PAINT.
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Delivers `event` to the content and applies what it asked for: repaint, cursor, timers.
    fn dispatch(&mut self, event: &Event) -> EventResult {
        let result = self.content.event(event);
        self.settle();
        result
    }

    fn settle(&mut self) {
        if self.content.take_repaint() {
            self.invalidate();
        }
        if let Some(cursor) = self.content.take_cursor() {
            self.cursor = cursor;
        }
        let mut timers = WindowTimers::new(self.hwnd, Rc::clone(&self.timers));
        self.content.flush_timers(&mut timers);
    }

    fn track_mouse(&mut self) {
        if self.tracking_mouse {
            return;
        }
        self.tracking_mouse = true;
        let mut request = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
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
    ) -> EventResult {
        let click_count = if kind == PointerEventKind::Down {
            let time = self.started.elapsed();
            button.map_or(0, |button| self.clicks.press(time, position, button))
        } else {
            0
        };
        self.dispatch(&Event::Pointer(PointerEvent {
            kind,
            button,
            position,
            pointer_type: PointerType::Mouse,
            click_count,
            mods: current_modifiers(),
        }))
    }

    /// Sets the pixel size, resizes the swap chain and lays the content out for it.
    fn resize(&mut self, width: i32, height: i32) {
        self.size_px = (width, height);
        if let Some(graphics) = self.graphics.as_mut() {
            graphics.surface.set_dpi(self.dpi as f32);
            let resized = graphics
                .surface
                .resize(width.max(1) as u32, height.max(1) as u32);
            self.recover_if_lost(&resized);
        }
        self.content.resize(self.size_px, self.dpi);
        self.paint();
    }

    fn paint(&mut self) {
        if self.painting || self.size_px.0 <= 0 || self.size_px.1 <= 0 {
            return;
        }
        self.painting = true;
        // A frame that meets a lost device is drawn once more on the rebuilt devices.
        for _ in 0..2 {
            let outcome = self.paint_once();
            if !self.recover_if_lost(&outcome) {
                break;
            }
        }
        self.painting = false;
        if self.content.needs_repaint_for_scenes() {
            self.invalidate();
        }
    }

    fn paint_once(&mut self) -> Result<(), WinError> {
        let Some(graphics) = self.graphics.as_mut() else {
            return Ok(());
        };
        let Graphics {
            surface,
            resources,
            assets,
            ..
        } = graphics;
        for (id, scene) in self.content.layout() {
            assets.register_scene(id, scene);
        }
        let content = &self.content;
        surface.paint(|target| {
            let mut canvas = D2dCanvas::new(target, assets, resources);
            content.paint(&mut canvas);
            canvas.finish()
        })
    }

    /// Rebuilds the device resources when `outcome` is a lost device; true when it was.
    fn recover_if_lost(&mut self, outcome: &Result<(), WinError>) -> bool {
        let lost = matches!(outcome, Err(error) if error.is_device_lost());
        if lost {
            self.recover_graphics();
        }
        lost
    }

    /// Rebuilds every device resource after a device loss; the next paint redraws. When the
    /// devices cannot be recreated the window stays without graphics (blank).
    fn recover_graphics(&mut self) {
        let (hwnd, size_px, dpi) = (self.hwnd, self.size_px, self.dpi);
        self.graphics = self.graphics.take().and_then(|old| {
            let Graphics {
                devices,
                surface,
                resources,
                assets,
            } = old;
            // The composition target must be released before a new one is made for this HWND.
            drop(surface);
            drop(resources);
            devices
                .recreate()
                .ok()
                .and_then(|devices| attach_surface(devices, hwnd, size_px, dpi, assets).ok())
        });
    }

    fn handle(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> (Option<LRESULT>, After) {
        match msg {
            WM_ERASEBKGND => (Some(LRESULT(1)), After::None),
            WM_PAINT => {
                // SAFETY: validates our own window so WM_PAINT stops repeating.
                unsafe {
                    let _ = ValidateRect(Some(self.hwnd), None);
                }
                self.paint();
                (Some(LRESULT(0)), After::None)
            }
            WM_SIZE => {
                if wparam.0 != SIZE_MINIMIZED as usize {
                    let point = point_of(lparam);
                    self.resize(point.x, point.y);
                }
                (Some(LRESULT(0)), After::None)
            }
            WM_DPICHANGED_AFTERPARENT => {
                self.dpi = window_dpi(self.hwnd);
                self.resize(self.size_px.0, self.size_px.1);
                (Some(LRESULT(0)), After::None)
            }
            WM_GETDLGCODE => (Some(LRESULT(DLGC_WANTARROWS)), After::None),
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
            WM_MOUSELEAVE => {
                self.tracking_mouse = false;
                self.pointer(PointerEventKind::Leave, None, Point::new(-1.0, -1.0));
                (Some(LRESULT(0)), After::None)
            }
            WM_MOUSEMOVE => {
                self.track_mouse();
                let point = point_of(lparam);
                let position = point_px_to_dip(point.x, point.y, self.scale());
                self.pointer(PointerEventKind::Move, None, position);
                (Some(LRESULT(0)), After::None)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN | WM_LBUTTONUP
            | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => self.button(msg, wparam, lparam),
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let position = self.client_point(point_of(lparam));
                let mut event =
                    wheel_event(delta, msg == WM_MOUSEHWHEEL, position, current_modifiers());
                // One notch scrolls the number of lines the user configured; the editor
                // assumes three.
                event.delta.y *= wheel_scroll_lines().min(100) as f32 / 3.0;
                self.dispatch(&Event::Wheel(event));
                (Some(LRESULT(0)), After::None)
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                let event = key_event(
                    (wparam.0 & 0xFFFF) as u16,
                    lparam.0,
                    true,
                    current_modifiers(),
                );
                if self.dispatch(&Event::Key(event)).handled {
                    return (Some(LRESULT(0)), After::None);
                }
                // SAFETY: pure queries of the message being handled.
                let (time, packed) = unsafe { (GetMessageTime(), GetMessagePos()) };
                let forwarded = MSG {
                    hwnd: self.hwnd,
                    message: msg,
                    wParam: wparam,
                    lParam: lparam,
                    time: time as u32,
                    pt: POINT {
                        x: i32::from((packed & 0xFFFF) as i16),
                        y: i32::from(((packed >> 16) & 0xFFFF) as i16),
                    },
                };
                // System keys keep their default behavior as well.
                let result = (msg == WM_KEYDOWN).then_some(LRESULT(0));
                (result, After::Translate(forwarded))
            }
            WM_SETFOCUS => {
                self.content.set_focused(true);
                self.settle();
                (Some(LRESULT(0)), After::None)
            }
            WM_KILLFOCUS => {
                self.content.set_focused(false);
                self.settle();
                (Some(LRESULT(0)), After::None)
            }
            WM_TIMER => {
                let fired = self.timers.borrow_mut().fire(wparam.0);
                if let Some(fired) = fired {
                    if fired.one_shot {
                        // SAFETY: kills the one-shot timer of our own window.
                        unsafe {
                            let _ = KillTimer(Some(self.hwnd), wparam.0);
                        }
                    }
                    self.dispatch(&Event::Timer(fired.id));
                }
                (Some(LRESULT(0)), After::None)
            }
            _ => (None, After::None),
        }
    }

    /// A mouse button press or release.
    fn button(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> (Option<LRESULT>, After) {
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
        let point = point_of(lparam);
        let position = point_px_to_dip(point.x, point.y, self.scale());
        let kind = if down {
            PointerEventKind::Down
        } else {
            PointerEventKind::Up
        };
        let mut after = After::None;
        if down {
            // Keep receiving the drag when the pointer leaves the window.
            if button == PointerButton::Left && !self.captured {
                // SAFETY: captures the mouse for our own window; released on button up.
                unsafe { SetCapture(self.hwnd) };
                self.captured = true;
            }
            // SAFETY: plain query.
            if unsafe { GetFocus() } != self.hwnd {
                after = After::Focus;
            }
        } else if button == PointerButton::Left && self.captured {
            self.captured = false;
            // SAFETY: releases the capture taken on the press.
            unsafe {
                let _ = ReleaseCapture();
            }
        }
        self.pointer(kind, Some(button), position);
        let result = if matches!(msg, WM_XBUTTONDOWN | WM_XBUTTONUP) {
            1
        } else {
            0
        };
        (Some(LRESULT(result)), after)
    }
}

/// The window procedure.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match catch_unwind(AssertUnwindSafe(|| dispatch(hwnd, msg, wparam, lparam))) {
        Ok(result) => result,
        // SAFETY: default processing with the original arguments.
        Err(_) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Routes one message to the window's [`WindowState`] (or to default processing).
fn dispatch(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: for WM_NCCREATE `lparam` points to the CREATESTRUCTW whose creation parameter
        // is the `RefCell<WindowState>` pointer passed by `PreviewWindow::create`, or null for a
        // window of this class that something else made (it then gets default processing only).
        unsafe {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            if let Some(cell) = (create.lpCreateParams as *const RefCell<WindowState>).as_ref()
                && let Ok(mut state) = cell.try_borrow_mut()
            {
                state.hwnd = hwnd;
            }
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    }
    // SAFETY: the pointer was stored in WM_NCCREATE and is cleared in WM_NCDESTROY; the cell
    // outlives the window because `PreviewWindow` destroys the window before dropping it.
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
    let (result, after) = match cell.try_borrow_mut() {
        Ok(mut state) => state.handle(msg, wparam, lparam),
        Err(_) => (None, After::None),
    };
    match after {
        After::None => {}
        After::Focus => {
            // SAFETY: the borrow is released; this sends WM_SETFOCUS back to the window.
            unsafe {
                let _ = SetFocus(Some(hwnd));
            }
        }
        After::Translate(forwarded) => {
            let frame = cell.try_borrow().ok().and_then(|state| state.frame.clone());
            if let Some(frame) = frame {
                // SAFETY: `forwarded` is a valid message structure for the call; the borrow is
                // released because the frame may send messages back. Whether the frame used
                // the key does not change what the window does with it.
                unsafe {
                    let _ = frame.TranslateAccelerator(&forwarded);
                }
            }
        }
    }
    match result {
        Some(result) => result,
        // SAFETY: default processing with the original arguments.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The graphics devices, swap chain and canvas assets of a window.
fn attach_graphics(
    hwnd: HWND,
    size_px: (i32, i32),
    dpi: u32,
    engine: &ssw_win::dwrite::DWriteEngine,
) -> Result<Graphics, WinError> {
    let devices = fault::check(GfxDevices::create(1))?;
    let wic = fault::check(Wic::new())?;
    let images = ImageStore::new(wic.clone());
    let assets = CanvasAssets::with_scene_renderer(images, engine.clone(), wic);
    let assets = fault::check(assets)?;
    attach_surface(devices, hwnd, size_px, dpi, assets)
}

/// The composition swap chain of `hwnd` on `devices` and the resources that draw into it,
/// with the canvas `assets` they work with.
fn attach_surface(
    devices: GfxDevices,
    hwnd: HWND,
    size_px: (i32, i32),
    dpi: u32,
    assets: CanvasAssets,
) -> Result<Graphics, WinError> {
    let (width, height) = (size_px.0.max(1) as u32, size_px.1.max(1) as u32);
    let created = WindowSurface::new(&devices, hwnd, width, height, dpi as f32);
    let surface = fault::check(created)?;
    let resources = TargetResources::new(devices.d2d_factory().clone(), devices.generation());
    Ok(Graphics {
        devices,
        surface,
        resources,
        assets,
    })
}

/// A preview child window, destroyed when dropped.
pub(crate) struct PreviewWindow {
    hwnd: HWND,
    state: Rc<RefCell<WindowState>>,
}

impl std::fmt::Debug for PreviewWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewWindow")
            .field("hwnd", &self.hwnd)
            .finish_non_exhaustive()
    }
}

impl PreviewWindow {
    /// Creates the child window of `parent` at `rect` (client coordinates of `parent`),
    /// showing `content`, and starts rendering. `frame` receives the keys the preview does not
    /// use.
    ///
    /// # Errors
    /// Fails when the window or its composition surface cannot be created; nothing is left
    /// behind then.
    pub(crate) fn create(
        parent: HWND,
        rect: RECT,
        content: PreviewContent,
        frame: Option<IPreviewHandlerFrame>,
    ) -> Result<Self, WinError> {
        acquire_class()?;
        let state = Rc::new(RefCell::new(WindowState {
            hwnd: HWND::default(),
            content,
            graphics: None,
            frame,
            dpi: DEFAULT_DPI,
            size_px: (0, 0),
            clicks: ClickTracker::new(
                // SAFETY: pure query.
                Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() })),
                CLICK_SLOP,
            ),
            started: Instant::now(),
            cursor: Cursor::Arrow,
            tracking_mouse: false,
            captured: false,
            timers: Rc::new(RefCell::new(TimerTable::default())),
            painting: false,
        }));
        // SAFETY: the creation parameter is the address of the `RefCell` inside the `Rc`, which
        // the returned `PreviewWindow` keeps alive until after the window is destroyed.
        let created = unsafe {
            CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP,
                CLASS_NAME,
                w!(""),
                WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                rect.left,
                rect.top,
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
                Some(parent),
                None,
                Some(module_handle()),
                Some(Rc::as_ptr(&state).cast::<c_void>()),
            )
        };
        let hwnd = match created.ctx("CreateWindowExW") {
            Ok(hwnd) => hwnd,
            Err(error) => {
                release_class();
                return Err(error);
            }
        };
        // From here on `window` owns the window: every early return below destroys it, so the
        // window procedure can never meet the state pointer after the `Rc` is gone.
        let window = Self { hwnd, state };
        window.initialize()?;
        Ok(window)
    }

    /// Reads DPI and size, attaches the composition surface and paints the first frame.
    fn initialize(&self) -> Result<(), WinError> {
        let hwnd = self.hwnd;
        let dpi = window_dpi(hwnd);
        let mut client = RECT::default();
        // SAFETY: valid out pointer.
        let queried = unsafe { GetClientRect(hwnd, &mut client) }.ctx("GetClientRect");
        fault::check(queried)?;
        let size_px = (client.right - client.left, client.bottom - client.top);
        // Nothing else holds the state of a window that was just created: the window procedure
        // only borrows it while it handles a message.
        let mut state = self.state.borrow_mut();
        state.hwnd = hwnd;
        state.dpi = dpi;
        state.size_px = size_px;
        let graphics = attach_graphics(hwnd, size_px, dpi, state.content.engine())?;
        state.graphics = Some(graphics);
        let wic = fault::check(Wic::new())?;
        let clipboard: SharedClipboard = Rc::new(RefCell::new(WindowsClipboard::new(hwnd, wic)));
        state.content.set_clipboard(clipboard);
        state.content.resize(size_px, dpi);
        state.paint();
        Ok(())
    }

    /// The window handle.
    pub(crate) fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Moves and sizes the window (client coordinates of its parent).
    pub(crate) fn set_rect(&self, rect: RECT) {
        // SAFETY: moves our own window; this sends WM_SIZE back, after no borrow is held here.
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                rect.left,
                rect.top,
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    /// Makes `parent` the parent of the window.
    pub(crate) fn set_parent(&self, parent: HWND) {
        // SAFETY: reparents our own window; a failure leaves it where it was.
        unsafe {
            let _ = SetParent(self.hwnd, Some(parent));
        }
    }

    /// Replaces the frame that receives the keys the preview does not use.
    pub(crate) fn set_frame(&self, frame: Option<IPreviewHandlerFrame>) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            state.frame = frame;
        }
    }

    /// Dresses the preview with new colors and font and repaints.
    pub(crate) fn set_visuals(&self, visuals: &Visuals, system: &SystemTheme) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            state.content.set_visuals(visuals, system);
            state.invalidate();
        }
    }

    /// Takes the keyboard focus.
    pub(crate) fn focus(&self) {
        // SAFETY: focuses our own window (sends WM_SETFOCUS, with no borrow held here).
        unsafe {
            let _ = SetFocus(Some(self.hwnd));
        }
    }

    /// How far the document is scrolled, in DIPs.
    pub(crate) fn scroll_y(&self) -> Option<f32> {
        self.state
            .try_borrow()
            .ok()
            .map(|state| state.content.scroll_y())
    }

    /// Renders the content into a bitmap of the given size (see
    /// [`PreviewContent::render`]); the window keeps its own layout.
    ///
    /// # Errors
    /// Fails when rendering fails or the window is in the middle of a message.
    pub(crate) fn render(
        &self,
        width_px: u32,
        height_px: u32,
        dpi: u32,
    ) -> Result<DecodedImage, WinError> {
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| WinError::Invalid("the preview window is busy"))?;
        let (size_px, own_dpi) = (state.size_px, state.dpi);
        let image = state.content.render(width_px, height_px, dpi);
        state.content.resize(size_px, own_dpi);
        image
    }
}

impl Drop for PreviewWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window created in `create` on its owning thread; when the host
        // already destroyed it (a child goes with its parent) this fails harmlessly.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        release_class();
    }
}

#[cfg(test)]
mod tests {
    use ssw_ui::editor::DocumentKind;
    use ssw_ui::services::TimerId;
    use ssw_win::com::ComGuard;
    use ssw_win::dwrite::DWriteEngine;
    use ssw_win::theme::{Win32Settings, read_system_theme};
    #[cfg(feature = "fault-injection")]
    use windows::Win32::UI::WindowsAndMessaging::{GW_CHILD, GetWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassInfoExW, GetCursor, GetParent, HTCAPTION, SendMessageW, SetTimer, WINDOW_EX_STYLE,
        WS_OVERLAPPEDWINDOW,
    };

    use super::*;

    /// The pane the windows of these tests fill, in pixels.
    const PANE: RECT = RECT {
        left: 0,
        top: 0,
        right: 600,
        bottom: 400,
    };

    /// Serializes the tests of one process that count users of the window class (nextest runs
    /// one test per process; `cargo test` shares the process).
    static SERIAL: Mutex<()> = Mutex::new(());

    /// What every test needs: exclusive use of the class counter and a COM apartment (for the
    /// text engine, WIC and the clipboard).
    struct Env {
        _com: ComGuard,
        _lock: MutexGuard<'static, ()>,
    }

    fn env() -> Env {
        let lock = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
        Env {
            _com: ComGuard::apartment_threaded().unwrap(),
            _lock: lock,
        }
    }

    /// A hidden, never shown top-level window: the host of a preview.
    struct Host(HWND);

    impl Host {
        fn new() -> Self {
            // SAFETY: creates a plain system "STATIC" window without `WS_VISIBLE`; it is never
            // shown or activated.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("STATIC"),
                    w!("ssw-preview window test host"),
                    WS_OVERLAPPEDWINDOW,
                    0,
                    0,
                    800,
                    600,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            Self(hwnd)
        }
    }

    impl Drop for Host {
        fn drop(&mut self) {
            // SAFETY: destroys the window created in `new` on its own thread.
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    fn make_content(text: &str) -> PreviewContent {
        let system = read_system_theme(&Win32Settings);
        let engine = DWriteEngine::new().unwrap();
        PreviewContent::new(
            text,
            DocumentKind::Markdown,
            &Visuals::default(),
            &system,
            engine,
        )
    }

    fn window_in(host: &Host, text: &str) -> PreviewWindow {
        PreviewWindow::create(host.0, PANE, make_content(text), None).unwrap()
    }

    /// A preview window in its host. Field order is drop order: the window goes first, then the
    /// host, then COM and the lock.
    struct Rig {
        window: PreviewWindow,
        host: Host,
        _env: Env,
    }

    fn rig(text: &str) -> Rig {
        let env = env();
        let host = Host::new();
        let window = window_in(&host, text);
        Rig {
            window,
            host,
            _env: env,
        }
    }

    fn send(hwnd: HWND, msg: u32, wparam: usize, lparam: isize) -> isize {
        // SAFETY: a synchronous message to a window of this thread; the arguments are plain
        // values, no pointers.
        unsafe { SendMessageW(hwnd, msg, Some(WPARAM(wparam)), Some(LPARAM(lparam))) }.0
    }

    /// `x | y << 16` as a mouse message's `lParam`.
    fn pack(x: i32, y: i32) -> isize {
        ((y as u16 as isize) << 16) | (x as u16 as isize)
    }

    fn class_registered() -> bool {
        let mut info = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            ..Default::default()
        };
        // SAFETY: a valid out structure with its size set and a constant class name.
        unsafe { GetClassInfoExW(Some(module_handle()), CLASS_NAME, &mut info) }.is_ok()
    }

    #[test]
    fn a_class_the_system_rejects_is_an_error_and_an_existing_one_is_accepted() {
        let error = register_class(&WNDCLASSEXW::default()).unwrap_err();
        assert!(
            matches!(
                &error,
                WinError::Os {
                    context: "RegisterClassExW",
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().starts_with("RegisterClassExW failed"));
        let _env = env();
        acquire_class().unwrap();
        register_class(&window_class()).unwrap();
        release_class();
    }

    #[test]
    fn the_class_lives_exactly_as_long_as_a_window_of_it_does() {
        let _env = env();
        let host = Host::new();
        assert!(!class_registered(), "no window, no class");
        let first = window_in(&host, "one\n");
        assert!(class_registered());
        let second = window_in(&host, "two\n");
        drop(first);
        assert!(
            class_registered(),
            "the second window still needs the class"
        );
        assert_eq!(send(second.hwnd(), WM_GETDLGCODE, 0, 0), DLGC_WANTARROWS);
        drop(second);
        assert!(
            !class_registered(),
            "the last window takes the class with it"
        );
        let again = window_in(&host, "three\n");
        assert!(class_registered(), "a later window registers it again");
        drop(again);
    }

    #[test]
    fn a_window_cannot_be_made_in_a_parent_that_is_gone() {
        let _env = env();
        let gone = Host::new().0;
        let error = PreviewWindow::create(gone, PANE, make_content("x\n"), None)
            .err()
            .unwrap();
        assert!(
            matches!(
                &error,
                WinError::Os {
                    context: "CreateWindowExW",
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(!class_registered(), "the failed window gave its use back");
    }

    #[test]
    fn graphics_cannot_be_attached_to_a_window_that_is_gone() {
        let _env = env();
        let gone = Host::new().0;
        let engine = DWriteEngine::new().unwrap();
        let error = attach_graphics(gone, (100, 100), 96, &engine)
            .err()
            .unwrap();
        assert!(
            matches!(
                &error,
                WinError::Os {
                    context: "create composition target",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_window_without_state_gets_default_processing() {
        let _env = env();
        let host = Host::new();
        acquire_class().unwrap();
        // SAFETY: creates a hidden child of the test host without a creation parameter; it is
        // destroyed below.
        let bare = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                w!(""),
                WS_CHILD,
                0,
                0,
                50,
                50,
                Some(host.0),
                None,
                Some(module_handle()),
                None,
            )
        }
        .unwrap();
        assert_eq!(
            send(bare, WM_GETDLGCODE, 0, 0),
            0,
            "no state, no claim on the arrow keys"
        );
        // SAFETY: destroys the window created above on its own thread.
        unsafe { DestroyWindow(bare) }.unwrap();
        release_class();
    }

    #[test]
    fn every_cursor_kind_is_a_different_stock_cursor() {
        let all = [
            Cursor::Arrow,
            Cursor::IBeam,
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
        ];
        let mut handles: Vec<usize> = all
            .iter()
            .map(|&cursor| {
                // SAFETY: loads a stock cursor.
                let loaded = unsafe { LoadCursorW(None, cursor_resource(cursor)) };
                loaded.unwrap().0 as usize
            })
            .collect();
        handles.sort_unstable();
        handles.dedup();
        assert_eq!(handles.len(), all.len());
    }

    #[test]
    fn the_pane_answers_its_own_background_and_dialog_code_and_paints_on_request() {
        let rig = rig("# Title\n\ntext\n");
        let hwnd = rig.window.hwnd();
        assert_eq!(
            send(hwnd, WM_ERASEBKGND, 0, 0),
            1,
            "no flicker from the system"
        );
        assert_eq!(send(hwnd, WM_GETDLGCODE, 0, 0), DLGC_WANTARROWS);
        assert_eq!(send(hwnd, WM_PAINT, 0, 0), 0);
        assert!(!rig.window.state.borrow().painting);
    }

    #[test]
    fn size_messages_resize_the_swap_chain_and_an_empty_client_paints_nothing() {
        let rig = rig("# Title\n\ntext\n");
        let hwnd = rig.window.hwnd();
        let sizes = || {
            let state = rig.window.state.borrow();
            let surface = state.graphics.as_ref().unwrap().surface.size_px();
            (state.size_px, surface)
        };
        assert_eq!(sizes(), ((600, 400), (600, 400)));
        send(hwnd, WM_SIZE, SIZE_MINIMIZED as usize, pack(0, 0));
        assert_eq!(
            sizes(),
            ((600, 400), (600, 400)),
            "a minimized window keeps its size"
        );
        send(hwnd, WM_SIZE, 0, pack(300, 200));
        assert_eq!(sizes(), ((300, 200), (300, 200)));
        send(hwnd, WM_SIZE, 0, pack(0, 0));
        assert_eq!(sizes(), ((0, 0), (1, 1)), "the swap chain keeps one pixel");
        send(hwnd, WM_SIZE, 0, pack(600, 400));
        assert_eq!(sizes(), ((600, 400), (600, 400)));
    }

    #[test]
    fn a_dpi_change_of_the_parent_makes_the_window_read_its_dpi_again() {
        let rig = rig("text\n");
        let hwnd = rig.window.hwnd();
        let own = rig.window.state.borrow().dpi;
        assert_eq!(own, window_dpi(hwnd));
        rig.window.state.borrow_mut().dpi = 4 * own;
        assert_eq!(send(hwnd, WM_DPICHANGED_AFTERPARENT, 0, 0), 0);
        assert_eq!(rig.window.state.borrow().dpi, own);
    }

    #[test]
    fn hovering_the_text_shows_the_i_beam_and_only_the_client_area_is_the_panes() {
        let rig = rig("Some text to hover over.\n");
        let hwnd = rig.window.hwnd();
        assert_eq!(rig.window.state.borrow().cursor, Cursor::Arrow);
        assert_eq!(send(hwnd, WM_MOUSEMOVE, 0, pack(300, 100)), 0);
        assert_eq!(rig.window.state.borrow().cursor, Cursor::IBeam);
        let client = ((WM_MOUSEMOVE as isize) << 16) | HTCLIENT as isize;
        assert_eq!(send(hwnd, WM_SETCURSOR, hwnd.0 as usize, client), 1);
        // SAFETY: plain queries of the thread's cursor and a stock cursor.
        let (current, ibeam) = unsafe { (GetCursor(), LoadCursorW(None, IDC_IBEAM).unwrap()) };
        assert_eq!(current, ibeam);
        let caption = HTCAPTION as isize;
        // SAFETY: default processing of a message to our own window.
        let default =
            unsafe { DefWindowProcW(hwnd, WM_SETCURSOR, WPARAM(hwnd.0 as usize), LPARAM(caption)) };
        assert_eq!(
            send(hwnd, WM_SETCURSOR, hwnd.0 as usize, caption),
            default.0,
            "the frame decides the cursor outside the client area"
        );
    }

    #[test]
    fn mouse_tracking_starts_once_and_restarts_after_the_pointer_left() {
        let rig = rig("text\n");
        let hwnd = rig.window.hwnd();
        let tracking = || rig.window.state.borrow().tracking_mouse;
        assert!(!tracking());
        send(hwnd, WM_MOUSEMOVE, 0, pack(100, 100));
        assert!(tracking());
        send(hwnd, WM_MOUSEMOVE, 0, pack(110, 100));
        assert!(tracking(), "one request covers the following moves");
        assert_eq!(send(hwnd, WM_MOUSELEAVE, 0, 0), 0);
        assert!(!tracking());
        send(hwnd, WM_MOUSEMOVE, 0, pack(120, 100));
        assert!(tracking(), "the next move asks again");
    }

    #[test]
    fn only_the_left_button_captures_the_mouse_and_x_buttons_answer_true() {
        let rig = rig("Hello selected world, a line of text.\n");
        let hwnd = rig.window.hwnd();
        let captured = || rig.window.state.borrow().captured;
        for (down, up) in [
            (WM_RBUTTONDOWN, WM_RBUTTONUP),
            (WM_MBUTTONDOWN, WM_MBUTTONUP),
        ] {
            assert_eq!(send(hwnd, down, 0, pack(30, 40)), 0);
            assert!(!captured(), "{down:#x} does not capture");
            assert_eq!(send(hwnd, up, 0, pack(30, 40)), 0);
        }
        for button in [1usize, 2] {
            assert_eq!(send(hwnd, WM_XBUTTONDOWN, button << 16, pack(30, 40)), 1);
            assert!(!captured());
            assert_eq!(send(hwnd, WM_XBUTTONUP, button << 16, pack(30, 40)), 1);
        }
        assert_eq!(
            send(hwnd, WM_LBUTTONUP, 0, pack(30, 40)),
            0,
            "a stray release"
        );
        assert!(!captured());
        assert_eq!(send(hwnd, WM_LBUTTONDOWN, 1, pack(30, 40)), 0);
        assert!(captured());
        assert_eq!(
            send(hwnd, WM_LBUTTONDOWN, 1, pack(35, 40)),
            0,
            "a second press"
        );
        assert!(captured());
        assert_eq!(send(hwnd, WM_LBUTTONUP, 0, pack(35, 40)), 0);
        assert!(
            !captured(),
            "releasing the left button gives the mouse back"
        );
    }

    #[test]
    fn dragging_below_the_pane_scrolls_on_the_autoscroll_timer_until_the_button_is_released() {
        let text: String = (0..80)
            .map(|i| format!("Paragraph {i} of the long document.\n\n"))
            .collect();
        let rig = rig(&text);
        let hwnd = rig.window.hwnd();
        let timers = Rc::clone(&rig.window.state.borrow().timers);
        send(hwnd, WM_LBUTTONDOWN, 1, pack(30, 40));
        send(hwnd, WM_MOUSEMOVE, 1, pack(300, 500));
        assert_eq!(
            timers.borrow().len(),
            1,
            "the drag started the autoscroll timer"
        );
        let os_id = (1..8)
            .find(|&id| timers.borrow_mut().fire(id).is_some())
            .expect("the OS id of the timer");
        let before = rig.window.scroll_y().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(send(hwnd, WM_TIMER, os_id, 0), 0);
        assert!(
            rig.window.scroll_y().unwrap() > before,
            "the tick scrolled the view"
        );
        send(hwnd, WM_LBUTTONUP, 0, pack(300, 500));
        assert!(
            timers.borrow().is_empty(),
            "releasing the button stops the timer"
        );
    }

    #[test]
    fn one_shot_timers_are_forgotten_when_they_fire_and_unknown_ones_are_ignored() {
        let rig = rig("text\n");
        let hwnd = rig.window.hwnd();
        let timers = Rc::clone(&rig.window.state.borrow().timers);
        assert_eq!(send(hwnd, WM_TIMER, 0xDEAD, 0), 0, "nobody set this timer");
        let repeating = timers.borrow_mut().register(TimerId::new(900), true);
        let once = timers.borrow_mut().register(TimerId::new(901), false);
        // SAFETY: real OS timers of our own window with the ids of the table; the window kills
        // the one-shot, the test the other.
        unsafe {
            let _ = SetTimer(Some(hwnd), repeating, 60_000, None);
            let _ = SetTimer(Some(hwnd), once, 60_000, None);
        }
        assert_eq!(send(hwnd, WM_TIMER, repeating, 0), 0);
        assert_eq!(timers.borrow().len(), 2, "a repeating timer stays");
        assert_eq!(send(hwnd, WM_TIMER, once, 0), 0);
        assert_eq!(timers.borrow().len(), 1, "a one-shot timer is forgotten");
        // SAFETY: kills timers of our own window; the one-shot is already gone.
        unsafe {
            assert!(KillTimer(Some(hwnd), once).is_err(), "the window killed it");
            assert!(KillTimer(Some(hwnd), repeating).is_ok());
        }
    }

    #[test]
    fn a_panic_while_handling_a_message_gets_default_processing_and_the_window_goes_on() {
        let rig = rig("text\n");
        let hwnd = rig.window.hwnd();
        let timers = Rc::clone(&rig.window.state.borrow().timers);
        // The WM_TIMER handler panics on its own borrow of the timer table.
        let held = timers.borrow_mut();
        assert_eq!(send(hwnd, WM_TIMER, 1, 0), 0);
        drop(held);
        assert_eq!(
            send(hwnd, WM_GETDLGCODE, 0, 0),
            DLGC_WANTARROWS,
            "the state was not left borrowed"
        );
    }

    #[test]
    fn a_window_in_the_middle_of_a_message_takes_no_new_state_and_renders_nothing() {
        let rig = rig("text\n");
        let system = read_system_theme(&Win32Settings);
        let mut visuals = Visuals::default();
        visuals.set_background(0x0010_2030);
        let plain = rig.window.state.borrow().content.background();
        let held = rig.window.state.borrow_mut();
        rig.window.set_frame(None);
        rig.window.set_visuals(&visuals, &system);
        assert_eq!(
            rig.window.scroll_y(),
            None,
            "the scroll offset is not readable now"
        );
        let refused = rig.window.render(100, 100, 96).err();
        assert!(
            matches!(
                refused,
                Some(WinError::Invalid("the preview window is busy"))
            ),
            "{refused:?}"
        );
        drop(held);
        assert_eq!(rig.window.state.borrow().content.background(), plain);
        rig.window.set_visuals(&visuals, &system);
        assert_eq!(
            rig.window.state.borrow().content.background(),
            visuals.background.unwrap()
        );
        assert_eq!(rig.window.scroll_y(), Some(0.0));
    }

    #[test]
    fn a_lost_device_is_rebuilt_on_the_next_generation_and_the_window_paints_again() {
        let rig = rig("# Title\n\nSome text\n");
        let mut state = rig.window.state.borrow_mut();
        let generation = |state: &WindowState| {
            state
                .graphics
                .as_ref()
                .map(|graphics| graphics.devices.generation())
        };
        assert_eq!(generation(&state), Some(1));
        assert!(!state.recover_if_lost(&Ok(())));
        assert!(!state.recover_if_lost(&Err(WinError::Invalid("not a device loss"))));
        assert_eq!(generation(&state), Some(1), "only a lost device is rebuilt");
        assert!(state.recover_if_lost(&Err(WinError::DeviceLost { context: "Present" })));
        assert_eq!(generation(&state), Some(2));
        let surface = state.graphics.as_ref().unwrap().surface.size_px();
        assert_eq!(
            surface,
            (600, 400),
            "the new swap chain has the window's size"
        );
        state.paint_once().unwrap();
        state.paint();
        assert!(!state.painting);
    }

    #[test]
    fn a_window_that_lost_its_graphics_stays_blank_when_asked_to_recover() {
        let rig = rig("text\n");
        let mut state = rig.window.state.borrow_mut();
        state.graphics = None;
        state.recover_graphics();
        assert!(state.graphics.is_none());
        state.paint_once().unwrap();
    }

    #[test]
    fn the_debug_form_names_the_window_handle() {
        let rig = rig("text\n");
        assert_eq!(
            format!("{:?}", rig.window),
            format!("PreviewWindow {{ hwnd: {:?}, .. }}", rig.window.hwnd())
        );
    }

    #[test]
    fn a_window_can_move_to_another_parent_and_take_the_focus() {
        let rig = rig("text\n");
        let hwnd = rig.window.hwnd();
        let other = Host::new();
        rig.window.set_parent(other.0);
        // SAFETY: a plain query of the parent of our own window.
        assert_eq!(unsafe { GetParent(hwnd) }.unwrap(), other.0);
        rig.window.set_parent(rig.host.0);
        // SAFETY: a plain query of the parent of our own window.
        assert_eq!(unsafe { GetParent(hwnd) }.unwrap(), rig.host.0);
        rig.window.focus();
        // SAFETY: a plain query of the thread's focus window.
        assert_eq!(unsafe { GetFocus() }, hwnd);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failure_while_creating_a_window_leaves_nothing_behind() {
        let _env = env();
        let host = Host::new();
        for n in 0.. {
            let content = make_content("Some $x^2$ text\n");
            fault::arm(n);
            let created = PreviewWindow::create(host.0, PANE, content, None);
            let tripped = fault::tripped();
            fault::disarm();
            if !tripped {
                assert!(
                    created.is_ok(),
                    "no failure was injected, so the window is made"
                );
                break;
            }
            let error = created.expect_err("the injected failure fails the creation");
            assert!(
                matches!(error, WinError::Invalid("injected fault")),
                "check {n}: {error:?}"
            );
            assert!(!class_registered(), "check {n}: the class was given back");
            // SAFETY: a plain query for the first child of our host.
            let child = unsafe { GetWindow(host.0, GW_CHILD) };
            assert!(child.is_err(), "check {n}: no window was left in the host");
        }
    }
}
