//! Crash handling (spec §4.6): a panic hook for the UI thread and an unhandled-exception filter
//! that write a minidump into `crashes\` and then run the emergency-save callback.
//! Worker-thread panics only chain to the previous hook (the job reports its own error).

use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::ThreadId;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MINIDUMP_TYPE,
    MiniDumpWithDataSegs, MiniDumpWithThreadInfo, MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
};

use crate::fs::{FsError, io};
use crate::recovery::now_millis;

/// Failure to write a minidump.
#[derive(Debug, thiserror::Error)]
pub enum CrashError {
    /// The dump file or folder could not be created.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// `MiniDumpWriteDump` failed.
    #[error("writing the minidump failed: {0}")]
    Dump(#[from] windows::core::Error),
}

struct Config {
    dir: PathBuf,
    emergency: Box<dyn Fn() + Send + Sync>,
    ui_thread: ThreadId,
}

static CONFIG: Mutex<Option<Arc<Config>>> = Mutex::new(None);
static HANDLING: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

fn config() -> Option<Arc<Config>> {
    CONFIG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Minidump of this process.
fn write_own_dump(dir: &Path, exception: *const EXCEPTION_POINTERS) -> Result<PathBuf, CrashError> {
    // SAFETY: plain query of the pseudo handle of the current process.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: plain query of the current process id.
    let process_id = unsafe { GetCurrentProcessId() };
    write_dump(dir, process, process_id, exception)
}

/// Writes a minidump of the process `process_id` (opened through `process`) into `dir`. The file
/// is named after `process_id` and the calling thread is reported as the faulting one, so both
/// must denote this process.
fn write_dump(
    dir: &Path,
    process: HANDLE,
    process_id: u32,
    exception: *const EXCEPTION_POINTERS,
) -> Result<PathBuf, CrashError> {
    io(std::fs::create_dir_all(dir), dir)?;
    let path = dir.join(format!("crash-{}-{process_id}.dmp", now_millis()));
    let file = io(std::fs::File::create(&path), &path)?;
    // SAFETY: plain query of the current thread id.
    let thread_id = unsafe { GetCurrentThreadId() };
    let information = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: thread_id,
        ExceptionPointers: exception.cast_mut(),
        ClientPointers: false.into(),
    };
    let exception_param = if exception.is_null() {
        None
    } else {
        Some(&raw const information)
    };
    let kind = MINIDUMP_TYPE(MiniDumpWithDataSegs.0 | MiniDumpWithThreadInfo.0);
    // SAFETY: the file handle stays open for the whole call, `information` (when passed) points
    // to a live struct whose exception pointers were handed to us by the OS or are null, and the
    // process handle is only read by the API (an invalid one makes the call fail).
    unsafe {
        MiniDumpWriteDump(
            process,
            process_id,
            HANDLE(file.as_raw_handle()),
            kind,
            exception_param,
            None,
            None,
        )
    }?;
    Ok(path)
}

/// Writes a minidump of the current process (no exception context) into `dir`.
///
/// # Errors
/// [`CrashError`] when the file cannot be created or the dump fails.
pub fn write_minidump(dir: &Path) -> Result<PathBuf, CrashError> {
    write_own_dump(dir, std::ptr::null())
}

fn handle_crash(exception: *const EXCEPTION_POINTERS) {
    if HANDLING.swap(true, Ordering::SeqCst) {
        // A crash while handling a crash: give up instead of recursing.
        return;
    }
    if let Some(config) = config() {
        // The dump is best effort; the emergency save must run regardless.
        let _ = write_own_dump(&config.dir, exception);
        (config.emergency)();
    }
    HANDLING.store(false, Ordering::SeqCst);
}

/// The `SetUnhandledExceptionFilter` callback: dump, emergency save, then let the process end
/// quietly.
///
/// # Safety
/// `info` must be null or point to the exception pointers the OS passed to the filter.
pub unsafe extern "system" fn unhandled_exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
    handle_crash(info);
    EXCEPTION_EXECUTE_HANDLER
}

/// Installs the crash handling. `dir` receives the minidumps; `emergency` saves dirty documents
/// (it runs on the crashing thread, after the dump). The calling thread is the UI thread: only
/// its panics trigger the dump and the emergency save. Calling `install` again replaces the
/// folder, callback and UI thread.
pub fn install(dir: PathBuf, emergency: impl Fn() + Send + Sync + 'static) {
    *CONFIG
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(Config {
        dir,
        emergency: Box::new(emergency),
        ui_thread: std::thread::current().id(),
    }));
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if config().is_some_and(|config| config.ui_thread == std::thread::current().id()) {
                handle_crash(std::ptr::null());
            }
        }));
    });
    // SAFETY: the filter is a valid `extern "system"` function that lives for the whole process.
    unsafe { SetUnhandledExceptionFilter(Some(unhandled_exception_filter)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rejected_dump_is_reported_as_a_dump_error() {
        let dir = tempfile::tempdir().unwrap();
        // The file is created, then the API cannot open a process that does not exist (process
        // ids are small multiples of four, so this one is never in use). A null handle alone is
        // not refused: the API then opens the process by its id.
        let error = write_dump(
            dir.path(),
            HANDLE::default(),
            u32::MAX - 3,
            std::ptr::null(),
        )
        .unwrap_err();
        assert!(matches!(error, CrashError::Dump(_)), "{error:?}");
        assert!(
            error
                .to_string()
                .starts_with("writing the minidump failed: "),
            "{error}"
        );
        let created: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(created.len(), 1, "{created:?}");
        assert!(created[0].starts_with("crash-"), "{created:?}");
    }
}
