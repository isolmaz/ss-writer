//! Taskbar button progress (`ITaskbarList3`) for long exports.
//!
//! [`Taskbar`] turns a [`Progress`] into the calls the taskbar needs (state, then value) and
//! skips requests that would change nothing. The two COM calls sit behind [`TaskbarApi`]:
//! [`SystemTaskbar`] makes them, tests substitute an implementation that records them.
//!
//! The taskbar forgets a window's progress when Explorer restarts. The window then receives the
//! registered message [`taskbar_button_created_message`]; the application calls
//! [`Taskbar::button_created`] and the last progress is shown again.

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Shell::{
    ITaskbarList3, TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPFLAG,
    TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW;
use windows::core::{IUnknown, w};

use crate::com::{OsResultExt, WinError};

/// What the taskbar button shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// No progress: the plain button.
    None,
    /// Work of unknown length: the marquee.
    Indeterminate,
    /// A green bar filled `completed / total`.
    Normal {
        /// Work done so far; values above `total` count as `total`.
        completed: u64,
        /// Total work; 0 shows the marquee because there is nothing to fill.
        total: u64,
    },
    /// A red bar filled `completed / total`: the work failed.
    Error {
        /// Work done before the failure; values above `total` count as `total`.
        completed: u64,
        /// Total work; 0 shows the marquee because there is nothing to fill.
        total: u64,
    },
}

impl Progress {
    /// The state flag and the bar value (`completed`, `total`) that show this progress.
    #[must_use]
    pub fn requests(self) -> (TBPFLAG, Option<(u64, u64)>) {
        match self {
            Self::None => (TBPF_NOPROGRESS, None),
            Self::Indeterminate | Self::Normal { total: 0, .. } | Self::Error { total: 0, .. } => {
                (TBPF_INDETERMINATE, None)
            }
            Self::Normal { completed, total } => (TBPF_NORMAL, Some((completed.min(total), total))),
            Self::Error { completed, total } => (TBPF_ERROR, Some((completed.min(total), total))),
        }
    }
}

/// The registered window message the taskbar sends when it (re)creates a window's button.
#[must_use]
pub fn taskbar_button_created_message() -> u32 {
    // SAFETY: a valid NUL-terminated message name.
    unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) }
}

/// The two taskbar calls.
pub trait TaskbarApi {
    /// Sets the progress state of `hwnd`'s button (`ITaskbarList3::SetProgressState`).
    ///
    /// # Errors
    /// The taskbar refused the request.
    fn set_state(&mut self, hwnd: HWND, state: TBPFLAG) -> windows::core::Result<()>;

    /// Sets the bar value of `hwnd`'s button (`ITaskbarList3::SetProgressValue`).
    ///
    /// # Errors
    /// The taskbar refused the request.
    fn set_value(&mut self, hwnd: HWND, completed: u64, total: u64) -> windows::core::Result<()>;
}

/// The real [`TaskbarApi`]: an `ITaskbarList3`. Create it on a COM-initialized thread and drop
/// it before that thread's [`crate::com::ComGuard`].
#[derive(Debug)]
pub struct SystemTaskbar {
    list: ITaskbarList3,
}

impl SystemTaskbar {
    /// Creates and initializes the taskbar list object.
    ///
    /// # Errors
    /// COM is not initialized on this thread or the shell object cannot be created.
    pub fn new() -> Result<Self, WinError> {
        // SAFETY: `TaskbarList` is the registered class of `ITaskbarList3`.
        let list: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None::<&IUnknown>, CLSCTX_INPROC_SERVER) }
                .ctx("create ITaskbarList3")?;
        // SAFETY: `list` is a freshly created, valid object.
        unsafe { list.HrInit() }.ctx("ITaskbarList3::HrInit")?;
        Ok(Self { list })
    }
}

impl TaskbarApi for SystemTaskbar {
    fn set_state(&mut self, hwnd: HWND, state: TBPFLAG) -> windows::core::Result<()> {
        // SAFETY: `list` is valid; the taskbar validates the window handle.
        unsafe { self.list.SetProgressState(hwnd, state) }
    }

    fn set_value(&mut self, hwnd: HWND, completed: u64, total: u64) -> windows::core::Result<()> {
        // SAFETY: `list` is valid; the taskbar validates the window handle.
        unsafe { self.list.SetProgressValue(hwnd, completed, total) }
    }
}

/// The progress display of one window's taskbar button.
#[derive(Debug)]
pub struct Taskbar<A: TaskbarApi = SystemTaskbar> {
    hwnd: HWND,
    api: A,
    /// What the button shows now (as far as this value knows).
    shown: Progress,
}

impl Taskbar {
    /// The taskbar button of `hwnd`, driven through the real taskbar.
    ///
    /// # Errors
    /// See [`SystemTaskbar::new`].
    pub fn new(hwnd: HWND) -> Result<Self, WinError> {
        Ok(Self::with_api(hwnd, SystemTaskbar::new()?))
    }
}

impl<A: TaskbarApi> Taskbar<A> {
    /// The taskbar button of `hwnd`, driven through `api`.
    #[must_use]
    pub fn with_api(hwnd: HWND, api: A) -> Self {
        Self {
            hwnd,
            api,
            shown: Progress::None,
        }
    }

    /// The progress last shown.
    #[must_use]
    pub fn progress(&self) -> Progress {
        self.shown
    }

    /// Shows `progress`. A request equal to what is shown makes no taskbar call.
    ///
    /// # Errors
    /// The taskbar refused a call; what is shown stays as it was.
    pub fn set(&mut self, progress: Progress) -> Result<(), WinError> {
        if progress == self.shown {
            return Ok(());
        }
        self.send(progress)?;
        self.shown = progress;
        Ok(())
    }

    /// Shows the current progress again: the taskbar created the button anew
    /// ([`taskbar_button_created_message`]) and knows nothing about it.
    ///
    /// # Errors
    /// The taskbar refused a call.
    pub fn button_created(&mut self) -> Result<(), WinError> {
        self.send(self.shown)
    }

    fn send(&mut self, progress: Progress) -> Result<(), WinError> {
        let (state, value) = progress.requests();
        self.api
            .set_state(self.hwnd, state)
            .ctx("ITaskbarList3::SetProgressState")?;
        if let Some((completed, total)) = value {
            self.api
                .set_value(self.hwnd, completed, total)
                .ctx("ITaskbarList3::SetProgressValue")?;
        }
        Ok(())
    }
}

impl<A: TaskbarApi> Drop for Taskbar<A> {
    fn drop(&mut self) {
        if self.shown != Progress::None {
            // Best effort: a progress bar must not outlive its owner, and a failure here has
            // nobody to report to.
            let _ = self.api.set_state(self.hwnd, TBPF_NOPROGRESS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_maps_to_taskbar_state_and_value() {
        assert_eq!(Progress::None.requests(), (TBPF_NOPROGRESS, None));
        assert_eq!(
            Progress::Indeterminate.requests(),
            (TBPF_INDETERMINATE, None)
        );
        assert_eq!(
            Progress::Normal {
                completed: 3,
                total: 10
            }
            .requests(),
            (TBPF_NORMAL, Some((3, 10)))
        );
        assert_eq!(
            Progress::Error {
                completed: 3,
                total: 10
            }
            .requests(),
            (TBPF_ERROR, Some((3, 10)))
        );
    }

    #[test]
    fn overshoot_is_clamped_and_an_empty_total_becomes_the_marquee() {
        assert_eq!(
            Progress::Normal {
                completed: 12,
                total: 10
            }
            .requests(),
            (TBPF_NORMAL, Some((10, 10)))
        );
        assert_eq!(
            Progress::Error {
                completed: 99,
                total: 1
            }
            .requests(),
            (TBPF_ERROR, Some((1, 1)))
        );
        for empty in [
            Progress::Normal {
                completed: 0,
                total: 0,
            },
            Progress::Error {
                completed: 5,
                total: 0,
            },
        ] {
            assert_eq!(empty.requests(), (TBPF_INDETERMINATE, None));
        }
    }
}
