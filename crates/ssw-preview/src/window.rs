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
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ssw_ui::geometry::Point;
use ssw_ui::input::{
    Cursor, Event, Modifiers, PointerButton, PointerEvent, PointerEventKind, PointerType,
};
use ssw_ui::widget::EventResult;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::canvas::{CanvasAssets, D2dCanvas, ImageStore, TargetResources};
use ssw_win::com::{OsResultExt, WinError};
use ssw_win::dpi::{DEFAULT_DPI, point_px_to_dip, scale_for_dpi};
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
use windows::Win32::UI::HiDpi::GetDpiForWindow;
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

/// Takes a use of the window class, registering it for the first user.
fn acquire_class() -> Result<(), WinError> {
    let mut users = CLASS_USERS.lock().unwrap_or_else(|e| e.into_inner());
    if *users == 0 {
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: module_handle(),
            // SAFETY: loading a stock cursor.
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            lpszClassName: CLASS_NAME,
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
    }
    *users += 1;
    Ok(())
}

/// Gives a use of the window class back, unregistering it for the last one.
fn release_class() {
    let mut users = CLASS_USERS.lock().unwrap_or_else(|e| e.into_inner());
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
            if let Err(error) = resized
                && error.is_device_lost()
            {
                self.recover_graphics();
            }
        }
        self.content.resize(self.size_px, self.dpi);
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
                // SAFETY: plain query on our own window.
                let dpi = unsafe { GetDpiForWindow(self.hwnd) };
                self.dpi = if dpi == 0 { DEFAULT_DPI } else { dpi };
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
        // is the `RefCell<WindowState>` pointer passed by `PreviewWindow::create`.
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
    let devices = GfxDevices::create(1)?;
    let surface = WindowSurface::new(
        &devices,
        hwnd,
        size_px.0.max(1) as u32,
        size_px.1.max(1) as u32,
        dpi as f32,
    )?;
    let resources = TargetResources::new(devices.d2d_factory().clone(), devices.generation());
    let wic = Wic::new()?;
    let assets =
        CanvasAssets::with_scene_renderer(ImageStore::new(wic.clone()), engine.clone(), wic)?;
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
        // SAFETY: plain query on the window just created.
        let dpi = match unsafe { GetDpiForWindow(hwnd) } {
            0 => DEFAULT_DPI,
            dpi => dpi,
        };
        let mut client = RECT::default();
        // SAFETY: valid out pointer.
        unsafe { GetClientRect(hwnd, &mut client) }.ctx("GetClientRect")?;
        let size_px = (client.right - client.left, client.bottom - client.top);
        let mut state = self
            .state
            .try_borrow_mut()
            .map_err(|_| WinError::Invalid("the preview window is busy"))?;
        state.hwnd = hwnd;
        state.dpi = dpi;
        state.size_px = size_px;
        let graphics = attach_graphics(hwnd, size_px, dpi, state.content.engine())?;
        state.graphics = Some(graphics);
        let clipboard: SharedClipboard =
            Rc::new(RefCell::new(WindowsClipboard::new(hwnd, Wic::new()?)));
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
        let (size_px, window_dpi) = (state.size_px, state.dpi);
        let image = state.content.render(width_px, height_px, dpi);
        state.content.resize(size_px, window_dpi);
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
