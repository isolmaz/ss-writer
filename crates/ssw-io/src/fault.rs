//! Fault injection for the error paths of system calls.
//!
//! Every Win32/COM call whose failure the crate handles but the operating system never causes
//! on a healthy machine goes through [`check`]. Normally [`check`] returns its argument
//! unchanged. With the `fault-injection` feature (tests only), a thread-local countdown armed
//! with [`arm`] makes the n-th checked call on the thread fail with `E_FAIL`, so a test can
//! sweep n = 0, 1, 2, … and run every error branch of an operation deterministically.
//!
//! [`check`] runs after the call it wraps: only wrap calls that are harmless for the sweep
//! (e.g. `IFileOperation::PerformOperations` on an empty queue).

/// Passes `result` through; with the `fault-injection` feature, replaces it by `E_FAIL` when
/// the countdown armed on this thread reaches the call.
#[inline(always)]
pub(crate) fn check<T>(result: windows::core::Result<T>) -> windows::core::Result<T> {
    #[cfg(feature = "fault-injection")]
    if injection::trip() {
        return Err(windows::Win32::Foundation::E_FAIL.into());
    }
    result
}

/// Fails *before* a call that cannot be undone (starting a thread): `Ok(())`, or with the
/// `fault-injection` feature `ERROR_NO_SYSTEM_RESOURCES` when the countdown armed on this
/// thread reaches the call.
#[inline(always)]
pub(crate) fn gate_io() -> std::io::Result<()> {
    #[cfg(feature = "fault-injection")]
    if injection::trip() {
        return Err(std::io::Error::from_raw_os_error(NO_SYSTEM_RESOURCES));
    }
    Ok(())
}

/// `ERROR_NO_SYSTEM_RESOURCES`, what the OS reports when it cannot create a thread.
#[cfg(feature = "fault-injection")]
pub const NO_SYSTEM_RESOURCES: i32 = 1450;

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
        use super::super::check;
        use super::{arm, disarm, tripped};

        #[test]
        fn the_armed_call_fails_once_and_the_others_pass_through() {
            arm(2);
            assert_eq!(check(Ok(1)).unwrap(), 1);
            assert_eq!(check(Ok(2)).unwrap(), 2);
            assert!(!tripped());
            let error = check(Ok(3)).unwrap_err();
            assert_eq!(error.code(), windows::Win32::Foundation::E_FAIL);
            assert!(tripped());
            assert_eq!(check(Ok(4)).unwrap(), 4, "a failure is injected once");
        }

        #[test]
        fn the_gate_fails_before_the_call_with_an_os_error() {
            arm(1);
            super::super::gate_io().unwrap();
            let error = super::super::gate_io().unwrap_err();
            assert_eq!(
                error.raw_os_error(),
                Some(super::super::NO_SYSTEM_RESOURCES)
            );
            assert!(tripped());
            super::super::gate_io().unwrap();
        }

        #[test]
        fn a_disarmed_thread_injects_nothing() {
            arm(0);
            disarm();
            assert_eq!(check(Ok(5)).unwrap(), 5);
            assert!(!tripped());
            let real: windows::core::Result<()> =
                Err(windows::Win32::Foundation::E_ACCESSDENIED.into());
            assert_eq!(
                check(real).unwrap_err().code(),
                windows::Win32::Foundation::E_ACCESSDENIED
            );
        }
    }
}
