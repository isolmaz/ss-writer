//! The hidden system caret.
//!
//! The editor draws its own caret; assistive technology (Narrator, Magnifier), the touch
//! keyboard and legacy IME windows locate the insertion point through the system caret of the
//! focused window (`GetGUIThreadInfo().rcCaret`, `OBJID_CARET`). [`SystemCaret`] keeps a system
//! caret at the position of the drawn caret and never shows it: it is created with
//! `CreateCaret`, moved with `SetCaretPos` and removed with `DestroyCaret`, but `ShowCaret` is
//! never called, so Windows does not draw or blink it.

use ssw_ui::geometry::Rect;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateCaret, DestroyCaret, GUITHREADINFO, GetGUIThreadInfo, SetCaretPos,
};

use crate::com::{OsResultExt, WinError};
use crate::dpi;

/// A hidden system caret tracking the drawn caret of one window.
///
/// A message queue has one caret. Create the caret when the window gets keyboard focus (the
/// first [`SystemCaret::place`]) and [`SystemCaret::release`] it when focus is lost, so
/// windows of one thread do not take each other's caret away.
#[derive(Debug)]
pub struct SystemCaret {
    hwnd: HWND,
    /// Pixel size of the caret created for the window, if one exists.
    size: Option<(i32, i32)>,
}

impl SystemCaret {
    /// A caret for `hwnd`; nothing is created until [`SystemCaret::place`].
    #[must_use]
    pub fn new(hwnd: HWND) -> Self {
        Self { hwnd, size: None }
    }

    /// True while this window owns the thread's system caret.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.size.is_some() && self.owns_caret()
    }

    /// Moves the caret to `rect`, the drawn caret in window DIPs, at `scale` physical pixels
    /// per DIP. Creates the caret on first use, and again when its pixel size changed or
    /// another window of the thread took it over.
    ///
    /// # Errors
    /// Fails when Windows refuses to create or move the caret (for example when the window
    /// belongs to another thread).
    pub fn place(&mut self, rect: Rect, scale: f32) -> Result<(), WinError> {
        let size = (
            dpi::dip_to_px(rect.width, scale).max(1),
            dpi::dip_to_px(rect.height, scale).max(1),
        );
        if self.size != Some(size) || !self.owns_caret() {
            // SAFETY: plain call; a null bitmap creates a solid caret. It replaces any caret
            // of the thread's queue. It is never shown.
            unsafe { CreateCaret(self.hwnd, None, size.0, size.1) }.ctx("CreateCaret")?;
            self.size = Some(size);
        }
        // SAFETY: plain call on the caret created above.
        unsafe { SetCaretPos(dpi::dip_to_px(rect.x, scale), dpi::dip_to_px(rect.y, scale)) }
            .ctx("SetCaretPos")
    }

    /// Destroys the caret if this window still owns it. Call when the window loses focus.
    pub fn release(&mut self) {
        if self.size.take().is_some() && self.owns_caret() {
            // SAFETY: the calling thread's caret belongs to this window (checked above).
            let _ = unsafe { DestroyCaret() };
        }
    }

    /// Whether the calling thread's caret belongs to this window.
    fn owns_caret(&self) -> bool {
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is initialized with its size and valid for the call.
        let queried = unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &mut info) };
        queried.is_ok() && info.hwndCaret == self.hwnd
    }
}

impl Drop for SystemCaret {
    fn drop(&mut self) {
        self.release();
    }
}
