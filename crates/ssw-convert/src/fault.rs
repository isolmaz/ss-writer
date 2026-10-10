//! Fault injection for the error paths of file-system calls.
//!
//! A healthy file system never fails some of the calls the bundle reader handles (reading the
//! next entry of a folder, its type or its size: the entry already carries them). Those calls go
//! through [`io`]. Normally [`io`] returns its argument unchanged. With the `fault-injection`
//! feature (tests only), a thread-local countdown armed with [`arm`] makes the n-th checked call
//! on the thread fail, so a test can sweep n = 0, 1, 2, … and run every error branch of an
//! operation deterministically.
//!
//! [`io`] runs after the call it wraps: only wrap calls that are harmless for the sweep.

/// Passes `result` through; with the `fault-injection` feature, replaces it by an I/O error with
/// the message [`INJECTED`] when the countdown armed on this thread reaches the call.
#[inline(always)]
pub(crate) fn io<T, E: From<std::io::Error>>(result: Result<T, E>) -> Result<T, E> {
    #[cfg(feature = "fault-injection")]
    if injection::trip() {
        return Err(std::io::Error::other(INJECTED).into());
    }
    result
}

/// Message of the error [`io`] injects.
#[cfg(feature = "fault-injection")]
pub const INJECTED: &str = "injected fault";

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
        use super::super::{INJECTED, io};
        use super::{arm, disarm, tripped};

        #[test]
        fn the_armed_call_fails_once_and_the_others_pass_through() {
            arm(2);
            assert_eq!(io::<_, std::io::Error>(Ok(1)).unwrap(), 1);
            assert_eq!(io::<_, std::io::Error>(Ok(2)).unwrap(), 2);
            assert!(!tripped());
            let error = io::<i32, std::io::Error>(Ok(3)).unwrap_err();
            assert_eq!(error.to_string(), INJECTED);
            assert!(tripped());
            assert_eq!(
                io::<_, std::io::Error>(Ok(4)).unwrap(),
                4,
                "a failure is injected once"
            );
        }

        #[test]
        fn a_disarmed_thread_injects_nothing() {
            arm(0);
            disarm();
            assert_eq!(io::<_, std::io::Error>(Ok(5)).unwrap(), 5);
            assert!(!tripped());
            let real: Result<(), std::io::Error> = Err(std::io::ErrorKind::NotFound.into());
            assert_eq!(
                io(real).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "the real error of a call is not replaced"
            );
        }
    }
}
