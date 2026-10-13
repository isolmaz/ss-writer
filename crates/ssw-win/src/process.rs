//! Waiting for another process to end.
//!
//! [`wait_for_exit`] is what the updater of an installed update needs: the application that
//! started it must be gone before the installer replaces its files.

use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

use crate::com::{OsResultExt, WinError};
use crate::fault::check;

/// How [`wait_for_exit`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitWait {
    /// The process has ended (or there is no process with that id any more).
    Exited,
    /// The process was still running when the time was up.
    TimedOut,
}

/// The longest wait [`wait_for_exit`] hands to the system: `INFINITE` is `u32::MAX`, which a
/// bounded wait must never reach.
const LONGEST_WAIT_MS: u32 = u32::MAX - 1;

/// Waits at most `timeout` for the process `pid` to end. A process id that does not exist
/// (the process ended and nothing holds it any more) counts as ended.
///
/// # Errors
/// [`WinError`] when the process exists but cannot be opened for waiting (another user's or an
/// elevated process) or the wait itself fails.
pub fn wait_for_exit(pid: u32, timeout: Duration) -> Result<ExitWait, WinError> {
    // SAFETY: plain value arguments; the returned handle is owned by this function and closed
    // below.
    let opened = check(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) });
    let process = match opened {
        Ok(process) => process,
        // The system reports "no such process" as an invalid parameter.
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            return Ok(ExitWait::Exited);
        }
        Err(error) => return Err(WinError::from_os("open the process", error)),
    };
    // SAFETY: `process` is the handle opened above and still open.
    let waited = unsafe { WaitForSingleObject(process, milliseconds(timeout)) };
    // The outcome is read before the handle is closed: closing may overwrite the last error.
    let outcome = classify(waited);
    // SAFETY: `process` was opened above and is closed exactly once.
    let _ = unsafe { CloseHandle(process) };
    outcome.ctx("wait for the process")
}

/// `timeout` in the milliseconds the system takes, never reaching `INFINITE`.
fn milliseconds(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).map_or(LONGEST_WAIT_MS, |ms| ms.min(LONGEST_WAIT_MS))
}

/// What the result of `WaitForSingleObject` on a process handle means.
fn classify(waited: WAIT_EVENT) -> windows::core::Result<ExitWait> {
    match waited {
        WAIT_OBJECT_0 => Ok(ExitWait::Exited),
        WAIT_TIMEOUT => Ok(ExitWait::TimedOut),
        // `WAIT_FAILED`: the reason is the calling thread's last error.
        _ => Err(windows::core::Error::from_thread()),
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, SetLastError, WAIT_FAILED};
    use windows::Win32::System::Threading::GetCurrentProcessId;

    use super::*;

    #[test]
    fn a_wait_result_is_read_as_ended_timed_out_or_failed() {
        assert_eq!(classify(WAIT_OBJECT_0).unwrap(), ExitWait::Exited);
        assert_eq!(classify(WAIT_TIMEOUT).unwrap(), ExitWait::TimedOut);
        // SAFETY: only sets the calling thread's last-error value.
        unsafe { SetLastError(ERROR_ACCESS_DENIED) };
        let error = classify(WAIT_FAILED).unwrap_err();
        assert_eq!(error.code(), ERROR_ACCESS_DENIED.to_hresult());
    }

    #[test]
    fn a_running_process_is_still_there_when_the_time_is_up() {
        // SAFETY: no arguments; returns the id of this process.
        let own = unsafe { GetCurrentProcessId() };
        assert_eq!(
            wait_for_exit(own, Duration::from_millis(1)).unwrap(),
            ExitWait::TimedOut
        );
    }

    #[test]
    fn a_timeout_becomes_milliseconds_below_the_infinite_wait() {
        assert_eq!(milliseconds(Duration::ZERO), 0);
        assert_eq!(milliseconds(Duration::from_millis(1500)), 1500);
        assert_eq!(
            milliseconds(Duration::from_millis(u64::from(u32::MAX))),
            u32::MAX - 1
        );
        assert_eq!(milliseconds(Duration::MAX), u32::MAX - 1);
    }

    #[test]
    fn a_process_that_does_not_exist_counts_as_ended() {
        assert_eq!(
            wait_for_exit(u32::MAX - 3, Duration::from_secs(30)).unwrap(),
            ExitWait::Exited
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_process_that_cannot_be_opened_is_reported_as_an_error() {
        // SAFETY: no arguments; returns the id of this process.
        let own = unsafe { GetCurrentProcessId() };
        crate::fault::arm(0);
        let error = wait_for_exit(own, Duration::from_millis(1)).unwrap_err();
        assert!(crate::fault::tripped());
        crate::fault::disarm();
        assert!(error.to_string().starts_with("open the process failed"));
    }
}
