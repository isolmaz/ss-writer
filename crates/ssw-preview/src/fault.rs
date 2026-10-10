//! Fault injection for the error paths of platform calls.
//!
//! Every call into the platform layer whose failure the preview handles goes through [`check`].
//! Normally [`check`] returns its argument unchanged. With the `fault-injection` feature (tests
//! only), a thread-local countdown armed with [`arm`] makes the n-th checked call on the thread
//! fail, so a test can sweep n = 0, 1, 2, … and run every error branch of an operation
//! deterministically, without depending on the operating system misbehaving.

use ssw_win::com::WinError;

/// Passes `result` through; with the `fault-injection` feature, replaces it by an error when the
/// countdown armed on this thread reaches the call.
#[inline(always)]
pub(crate) fn check<T>(result: Result<T, WinError>) -> Result<T, WinError> {
    #[cfg(feature = "fault-injection")]
    if injection::trip() {
        return Err(WinError::Invalid("injected fault"));
    }
    result
}

/// A failure point in front of a platform call that has no `Result` to pass through (it reports
/// failure by a sentinel): fails like [`check`] when armed, before the call runs.
#[inline(always)]
pub(crate) fn point() -> Result<(), WinError> {
    check(Ok(()))
}

#[cfg(feature = "fault-injection")]
pub use injection::{arm, disarm, tripped};

#[cfg(feature = "fault-injection")]
mod injection {
    use std::cell::Cell;

    thread_local! {
        /// Checked calls left before the injected failure; `None` when disarmed.
        static COUNTDOWN: Cell<Option<u32>> = const { Cell::new(None) };
        /// Whether the armed failure was injected.
        static TRIPPED: Cell<bool> = const { Cell::new(false) };
    }

    /// Makes the checked call number `n` (0-based, counted from now) on this thread fail.
    pub fn arm(n: u32) {
        COUNTDOWN.set(Some(n));
        TRIPPED.set(false);
    }

    /// Stops injecting failures on this thread.
    pub fn disarm() {
        COUNTDOWN.set(None);
    }

    /// Whether the failure armed with [`arm`] was injected (the operation reached call `n`).
    #[must_use]
    pub fn tripped() -> bool {
        TRIPPED.get()
    }

    pub(super) fn trip() -> bool {
        match COUNTDOWN.get() {
            Some(0) => {
                COUNTDOWN.set(None);
                TRIPPED.set(true);
                true
            }
            Some(left) => {
                COUNTDOWN.set(Some(left - 1));
                false
            }
            None => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use ssw_win::com::WinError;

        use super::super::check;
        use super::{arm, disarm, tripped};

        #[test]
        fn the_armed_call_fails_once_and_the_others_pass_through() {
            arm(2);
            assert_eq!(check(Ok(1)).unwrap(), 1);
            assert_eq!(check(Ok(2)).unwrap(), 2);
            assert!(!tripped());
            let error = check(Ok(3)).unwrap_err();
            assert!(matches!(error, WinError::Invalid(_)), "{error:?}");
            assert!(tripped());
            assert_eq!(check(Ok(4)).unwrap(), 4, "a failure is injected once");
        }

        #[test]
        fn a_disarmed_thread_injects_nothing_and_real_errors_pass_through() {
            arm(0);
            disarm();
            assert_eq!(check(Ok(5)).unwrap(), 5);
            assert!(!tripped());
            let real: Result<(), WinError> = Err(WinError::DeviceLost { context: "real" });
            let error = check(real).unwrap_err();
            assert!(error.is_device_lost(), "{error:?}");
        }
    }
}
