//! Fault injection for the error paths of system calls.
//!
//! Every Win32/COM call whose failure the crate handles goes through [`check`]. Normally
//! [`check`] returns its argument unchanged. With the `fault-injection` feature (tests only),
//! a thread-local countdown armed with [`arm`] makes the n-th checked call on the thread fail
//! (with `E_FAIL`, or the code of [`arm_with`]), so a test can sweep n = 0, 1, 2, … ([`sweep`])
//! and run every error branch of an operation deterministically, without depending on the
//! operating system misbehaving.

/// Passes `result` through; with the `fault-injection` feature, replaces it by the injected
/// failure when the countdown armed on this thread reaches the call.
#[inline(always)]
pub(crate) fn check<T>(result: windows::core::Result<T>) -> windows::core::Result<T> {
    #[cfg(feature = "fault-injection")]
    if let Some(code) = injection::trip() {
        return Err(code.into());
    }
    result
}

/// The operation a failure belongs to, as [`crate::com::OsResultExt::ctx`] names it in the
/// message ("create WIC bitmap" for "create WIC bitmap failed: ..."); `None` for an outcome
/// that is no error. For the sweeps of unit tests: the list of steps an operation reports is
/// the list of its failure points.
#[cfg(all(test, feature = "fault-injection"))]
pub(crate) fn failed_step<T>(outcome: Result<T, crate::com::WinError>) -> Option<String> {
    outcome.err().map(|error| {
        error
            .to_string()
            .split(" failed: ")
            .next()
            .unwrap_or_default()
            .to_owned()
    })
}

/// The [`failed_step`] results of a sweep in which every injected failure was reported as the
/// failure of its own step: `names` lists the steps in call order.
#[cfg(all(test, feature = "fault-injection"))]
pub(crate) fn all_failed(names: &[&str]) -> Vec<Option<String>> {
    names.iter().map(|step| Some((*step).to_owned())).collect()
}

#[cfg(feature = "fault-injection")]
pub use injection::{arm, arm_from, arm_with, disarm, sweep, sweep_with, tripped};

#[cfg(feature = "fault-injection")]
mod injection {
    use std::cell::Cell;

    use windows::Win32::Foundation::E_FAIL;
    use windows::core::HRESULT;

    thread_local! {
        /// Checked calls left before the injected failure; `None` when disarmed.
        static COUNTDOWN: Cell<Option<u32>> = const { Cell::new(None) };
        /// Whether the armed failure was injected.
        static TRIPPED: Cell<bool> = const { Cell::new(false) };
        /// The failure injected.
        static CODE: Cell<HRESULT> = const { Cell::new(E_FAIL) };
        /// Whether every call from the armed one on fails, not just that one.
        static STICKY: Cell<bool> = const { Cell::new(false) };
    }

    fn start(n: u32, code: HRESULT, sticky: bool) {
        COUNTDOWN.set(Some(n));
        CODE.set(code);
        STICKY.set(sticky);
        TRIPPED.set(false);
    }

    /// Makes the checked call number `n` (0-based, counted from now) on this thread fail with
    /// `E_FAIL`.
    pub fn arm(n: u32) {
        start(n, E_FAIL, false);
    }

    /// Like [`arm`], but the call fails with `code` (a device-lost code, say).
    pub fn arm_with(n: u32, code: HRESULT) {
        start(n, code, false);
    }

    /// Makes the checked call number `n` and every one after it fail with `E_FAIL` until
    /// [`disarm`] (an operation that retries, or falls back, fails as a whole).
    pub fn arm_from(n: u32) {
        start(n, E_FAIL, true);
    }

    /// Stops injecting failures on this thread.
    pub fn disarm() {
        COUNTDOWN.set(None);
        STICKY.set(false);
    }

    /// Whether the failure armed with [`arm`] was injected (the operation reached call `n`).
    #[must_use]
    pub fn tripped() -> bool {
        TRIPPED.get()
    }

    /// Runs `operation` with `E_FAIL` injected at its checked call 0, then 1, 2, … until a run
    /// gets through without reaching the armed call. Returns the outcomes of the runs that were
    /// hit, in call order: element `n` is what the operation made of the failure of call `n`.
    pub fn sweep<T>(operation: impl FnMut() -> T) -> Vec<T> {
        sweep_with(E_FAIL, operation)
    }

    /// [`sweep`] with the failure `code`.
    pub fn sweep_with<T>(code: HRESULT, mut operation: impl FnMut() -> T) -> Vec<T> {
        let mut outcomes = Vec::new();
        for call in 0.. {
            arm_with(call, code);
            let outcome = operation();
            let hit = tripped();
            disarm();
            if !hit {
                break;
            }
            outcomes.push(outcome);
        }
        outcomes
    }

    /// The failure to inject into the checked call being made now, if it is the armed one.
    pub(super) fn trip() -> Option<HRESULT> {
        match COUNTDOWN.get() {
            Some(0) => {
                if !STICKY.get() {
                    COUNTDOWN.set(None);
                }
                TRIPPED.set(true);
                Some(CODE.get())
            }
            Some(left) => {
                COUNTDOWN.set(Some(left - 1));
                None
            }
            None => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::check;
        use super::{arm, arm_from, arm_with, disarm, sweep, sweep_with, tripped};
        use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL};

        #[test]
        fn the_armed_call_fails_once_and_the_others_pass_through() {
            arm(2);
            assert_eq!(check(Ok(1)).unwrap(), 1);
            assert_eq!(check(Ok(2)).unwrap(), 2);
            assert!(!tripped());
            let error = check(Ok(3)).unwrap_err();
            assert_eq!(error.code(), E_FAIL);
            assert!(tripped());
            assert_eq!(check(Ok(4)).unwrap(), 4, "a failure is injected once");
        }

        #[test]
        fn a_disarmed_thread_injects_nothing() {
            arm(0);
            disarm();
            assert_eq!(check(Ok(5)).unwrap(), 5);
            assert!(!tripped());
            let real: windows::core::Result<()> = Err(E_ACCESSDENIED.into());
            assert_eq!(check(real).unwrap_err().code(), E_ACCESSDENIED);
        }

        #[test]
        fn the_injected_failure_carries_the_code_asked_for() {
            arm_with(1, E_ACCESSDENIED);
            assert!(check(Ok(())).is_ok());
            assert_eq!(check(Ok(())).unwrap_err().code(), E_ACCESSDENIED);
            disarm();
        }

        #[test]
        fn a_sticky_failure_fails_every_call_from_the_armed_one_until_disarmed() {
            arm_from(1);
            assert!(check(Ok(0)).is_ok());
            assert!(check(Ok(1)).is_err());
            assert!(check(Ok(2)).is_err());
            assert!(tripped());
            disarm();
            assert!(check(Ok(3)).is_ok());
        }

        #[test]
        fn a_sweep_hits_every_checked_call_of_the_operation_once() {
            let outcomes = sweep(|| {
                [
                    check(Ok(())).is_err(),
                    check(Ok(())).is_err(),
                    check(Ok(())).is_err(),
                ]
            });
            assert_eq!(
                outcomes,
                [
                    [true, false, false],
                    [false, true, false],
                    [false, false, true]
                ]
            );
            // The sweep leaves the thread disarmed.
            assert!(check(Ok(())).is_ok());
        }

        #[test]
        fn a_sweep_of_an_operation_without_checked_calls_has_nothing_to_report() {
            assert!(sweep_with(E_ACCESSDENIED, || 7).is_empty());
        }

        #[test]
        fn the_failed_step_is_the_context_of_the_error() {
            use crate::com::WinError;

            let failed: Result<(), WinError> =
                Err(WinError::from_os("create bitmap", E_FAIL.into()));
            assert_eq!(
                super::super::failed_step(failed).as_deref(),
                Some("create bitmap")
            );
            assert_eq!(super::super::failed_step(Ok(1)), None);
        }
    }
}
