//! Helpers shared by the integration tests of this crate (`mod support;`).
//!
//! `cargo nextest` runs every test in its own process, in parallel: an in-process `Mutex` does
//! not keep two tests away from a resource the whole machine shares (the clipboard, the user's
//! spelling dictionaries, the Jump List of an application id, the file associations). A
//! [`MachineLock`] does: it is a named system mutex.

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_ABANDONED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateMutexW, INFINITE, ReleaseMutex, WaitForSingleObject,
};
use windows::core::HSTRING;

/// Exclusive use of one shared resource of the machine, across threads and processes, for as
/// long as the value lives. Acquire it first in a test and drop it last (declare it before every
/// other guard): the resource is back in its original state by then.
///
/// The lock belongs to the thread that acquired it, so it must be dropped on that thread (a
/// test's own thread, as every guard of a test is).
#[derive(Debug)]
pub struct MachineLock(HANDLE);

impl MachineLock {
    /// Waits until no other thread or process holds the lock named `resource`. A holder that
    /// ended without releasing it (a crashed test process) does not block anybody.
    pub fn acquire(resource: &str) -> Self {
        let name = HSTRING::from(format!(r"Local\ssw-win-tests-{resource}"));
        // SAFETY: `name` is a valid NUL-terminated string; default security attributes.
        let handle = unsafe { CreateMutexW(None, false, &name) }.expect("CreateMutexW");
        // SAFETY: `handle` is the mutex that was just created or opened.
        let outcome = unsafe { WaitForSingleObject(handle, INFINITE) };
        assert!(
            outcome == WAIT_OBJECT_0 || outcome == WAIT_ABANDONED,
            "waiting for the {resource} lock failed: {outcome:?}"
        );
        Self(handle)
    }
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        // SAFETY: the handle is the mutex this value acquired on this thread; it is released
        // and closed exactly once.
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}
