//! Windows implementations of the [`ssw_ui::services`] traits.

pub mod clipboard;
pub mod clock;
pub mod dialogs;
pub mod shell;
pub mod timers;

use ssw_ui::services::ServiceError;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

use crate::com::WinError;

/// Converts a platform error into the UI-facing [`ServiceError`] for `service`.
#[must_use]
pub fn service_error(service: &'static str, error: &WinError) -> ServiceError {
    ServiceError::Failed {
        service,
        message: error.to_string(),
    }
}

/// The window of the calling thread that is active. Dialogs and shell calls the caller gave no
/// owner belong to it, so the application's windows are disabled while one is up. `None` while
/// no window of the thread is active (before the first window exists, or while another program
/// is in front).
pub(crate) fn active_window() -> Option<HWND> {
    // SAFETY: a plain query of the calling thread's input state.
    let active = unsafe { GetActiveWindow() };
    (!active.is_invalid()).then_some(active)
}

/// Shorthand for a failed OS call inside a service.
pub(crate) fn failed(service: &'static str, error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Failed {
        service,
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_carry_the_service_name_and_message() {
        let error = service_error("clipboard", &WinError::Invalid("boom"));
        assert_eq!(
            error,
            ServiceError::Failed {
                service: "clipboard",
                message: "boom".to_owned()
            }
        );
        assert_eq!(
            failed("shell", "nope"),
            ServiceError::Failed {
                service: "shell",
                message: "nope".to_owned()
            }
        );
    }
}
