//! The hidden system caret against the real window manager: `CreateCaret` / `SetCaretPos` /
//! `DestroyCaret` observed through `GetGUIThreadInfo` and `GetCaretPos`, the way assistive
//! technology reads the insertion point. Windows are created on the test thread and never shown,
//! activated or focused (tool windows of the `STATIC` class at (-32000, -32000)).

use ssw_ui::geometry::Rect;
use ssw_win::caret::SystemCaret;
use ssw_win::com::WinError;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, GUITHREADINFO, GetCaretPos, GetGUIThreadInfo, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

/// A window that is created and never shown.
struct HiddenWindow(HWND);

impl HiddenWindow {
    fn new() -> Self {
        // SAFETY: creates a window of a system class on this thread; it is never shown.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                -32000,
                -32000,
                320,
                240,
                None,
                None,
                None,
                None,
            )
        }
        .expect("the hidden window is created");
        Self(hwnd)
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window this struct created on this thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// The caret of this thread's queue as `GetGUIThreadInfo` reports it:
/// `(owner, (left, top, right, bottom), blinking)`.
fn reported() -> (HWND, (i32, i32, i32, i32), bool) {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is initialized with its size and valid for the call.
    unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &mut info) }.expect("GetGUIThreadInfo");
    let rect = info.rcCaret;
    (
        info.hwndCaret,
        (rect.left, rect.top, rect.right, rect.bottom),
        info.flags.0 & 1 != 0,
    )
}

fn caret_position() -> (i32, i32) {
    let mut point = POINT::default();
    // SAFETY: `point` is a valid POINT.
    unsafe { GetCaretPos(&mut point) }.expect("GetCaretPos");
    (point.x, point.y)
}

#[test]
fn place_creates_a_hidden_caret_at_the_drawn_caret() {
    let window = HiddenWindow::new();
    let mut caret = SystemCaret::new(window.0);
    assert!(!caret.is_active());
    caret
        .place(Rect::new(10.0, 20.0, 2.0, 18.0), 1.0)
        .expect("the caret is created");
    assert!(caret.is_active());
    let (owner, rect, blinking) = reported();
    assert_eq!(owner, window.0);
    assert_eq!(rect, (10, 20, 12, 38), "client coordinates in pixels");
    assert_eq!(caret_position(), (10, 20));
    // ShowCaret is never called: Windows neither draws nor blinks it.
    assert!(!blinking);
}

#[test]
fn moving_keeps_the_caret_and_a_new_size_recreates_it() {
    let window = HiddenWindow::new();
    let mut caret = SystemCaret::new(window.0);
    caret
        .place(Rect::new(10.0, 20.0, 2.0, 18.0), 1.0)
        .expect("create");
    caret
        .place(Rect::new(30.0, 40.0, 2.0, 18.0), 1.0)
        .expect("move");
    assert_eq!(reported().1, (30, 40, 32, 58));
    // A different line height (a heading) is a different caret.
    caret
        .place(Rect::new(30.0, 40.0, 2.0, 24.0), 1.0)
        .expect("resize");
    assert_eq!(reported().1, (30, 40, 32, 64));
}

#[test]
fn dips_are_scaled_to_pixels_and_degenerate_rectangles_get_a_one_pixel_caret() {
    let window = HiddenWindow::new();
    let mut caret = SystemCaret::new(window.0);
    caret
        .place(Rect::new(10.0, 20.0, 2.0, 18.0), 1.5)
        .expect("scaled");
    assert_eq!(reported().1, (15, 30, 18, 57));
    caret
        .place(Rect::new(5.0, 5.0, 0.0, 0.0), 1.0)
        .expect("empty");
    assert_eq!(reported().1, (5, 5, 6, 6));
}

#[test]
fn release_and_drop_remove_the_caret() {
    let window = HiddenWindow::new();
    let mut caret = SystemCaret::new(window.0);
    caret
        .place(Rect::new(1.0, 2.0, 2.0, 10.0), 1.0)
        .expect("create");
    caret.release();
    assert!(!caret.is_active());
    assert_eq!(reported().0, HWND::default());
    // Releasing again, or without a caret, is a no-op.
    caret.release();
    // The caret can be created again.
    caret
        .place(Rect::new(1.0, 2.0, 2.0, 10.0), 1.0)
        .expect("recreate");
    assert_eq!(reported().0, window.0);
    drop(caret);
    assert_eq!(reported().0, HWND::default());
}

#[test]
fn a_window_never_destroys_the_caret_another_window_of_the_thread_took() {
    let first = HiddenWindow::new();
    let second = HiddenWindow::new();
    let mut a = SystemCaret::new(first.0);
    let mut b = SystemCaret::new(second.0);
    a.place(Rect::new(1.0, 1.0, 2.0, 10.0), 1.0).expect("a");
    b.place(Rect::new(3.0, 3.0, 2.0, 10.0), 1.0)
        .expect("b takes over");
    assert!(!a.is_active());
    assert!(b.is_active());
    // The first window releases late: the second window's caret survives.
    a.release();
    assert_eq!(reported().0, second.0);
    // The first window gets its caret back by placing it again.
    a.place(Rect::new(1.0, 1.0, 2.0, 10.0), 1.0)
        .expect("a again");
    assert_eq!(reported().0, first.0);
    drop(a);
    drop(b);
    assert_eq!(reported().0, HWND::default());
}

#[test]
fn a_window_of_another_thread_cannot_own_the_caret() {
    let (sender, receiver) = std::sync::mpsc::channel::<isize>();
    let (done_sender, done_receiver) = std::sync::mpsc::channel::<()>();
    let thread = std::thread::spawn(move || {
        let window = HiddenWindow::new();
        sender.send(window.0.0 as isize).expect("handle sent");
        // Keep the foreign window alive until the test is done with it.
        let _ = done_receiver.recv();
    });
    let raw = receiver.recv().expect("handle received");
    let mut caret = SystemCaret::new(HWND(raw as *mut core::ffi::c_void));
    let error = caret
        .place(Rect::new(1.0, 1.0, 2.0, 10.0), 1.0)
        .expect_err("another thread's window cannot get this thread's caret");
    assert!(
        matches!(
            error,
            WinError::Os {
                context: "CreateCaret",
                ..
            }
        ),
        "{error:?}"
    );
    assert!(!caret.is_active());
    done_sender.send(()).expect("thread released");
    thread.join().expect("window thread ends");
}
