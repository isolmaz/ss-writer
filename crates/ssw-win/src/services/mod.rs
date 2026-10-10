//! Windows implementations of the [`ssw_ui::services`] traits.

pub mod clipboard;
pub mod clock;
pub mod dialogs;
pub mod shell;
pub mod timers;

use ssw_ui::services::ServiceError;

use crate::com::WinError;

/// Converts a platform error into the UI-facing [`ServiceError`] for `service`.
#[must_use]
pub fn service_error(service: &'static str, error: &WinError) -> ServiceError {
    ServiceError::Failed {
        service,
        message: error.to_string(),
    }
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
