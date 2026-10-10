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

/// Minidump of this process in `dir`, in a file named after `stamp` (the time in milliseconds)
/// and the process id.
fn write_own_dump(
    dir: &Path,
    stamp: u64,
    exception: *const EXCEPTION_POINTERS,
) -> Result<PathBuf, CrashError> {
    io(std::fs::create_dir_all(dir), dir)?;
    // SAFETY: plain query of the pseudo handle of the current process.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: plain query of the current process id.
    let process_id = unsafe { GetCurrentProcessId() };
    let path = dir.join(format!("crash-{stamp}-{process_id}.dmp"));
    write_dump(&path, process, process_id, exception)?;
    Ok(path)
}

/// Creates the file `path` and writes a minidump of the process `process_id` (opened through
/// `process`) into it. The calling thread is reported as the faulting one, so `process` and
/// `process_id` must denote this process.
fn write_dump(
    path: &Path,
    process: HANDLE,
    process_id: u32,
    exception: *const EXCEPTION_POINTERS,
) -> Result<(), CrashError> {
    let file = io(std::fs::File::create(path), path)?;
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
    Ok(())
}

/// Writes a minidump of the current process (no exception context) into `dir`.
///
/// # Errors
/// [`CrashError`] when the file cannot be created or the dump fails.
pub fn write_minidump(dir: &Path) -> Result<PathBuf, CrashError> {
    write_own_dump(dir, now_millis(), std::ptr::null())
}

fn handle_crash(exception: *const EXCEPTION_POINTERS) {
    if HANDLING.swap(true, Ordering::SeqCst) {
        // A crash while handling a crash: give up instead of recursing.
        return;
    }
    if let Some(config) = config() {
        // The dump is best effort; the emergency save must run regardless.
        let _ = write_own_dump(&config.dir, now_millis(), exception);
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

    /// A process id that is never in use: ids are small multiples of four.
    const NO_SUCH_PROCESS: u32 = u32::MAX - 3;

    #[test]
    fn a_rejected_dump_is_reported_as_a_dump_error() {
        let dir = tempfile::tempdir().unwrap();
        // The file is created, then the API cannot open a process that does not exist. A null
        // handle alone is not refused: the API then opens the process by its id. A dump that
        // reports an exception is refused the same way.
        let exception = EXCEPTION_POINTERS {
            ExceptionRecord: std::ptr::null_mut(),
            ContextRecord: std::ptr::null_mut(),
        };
        for (name, pointers) in [
            ("plain.dmp", std::ptr::null()),
            ("exception.dmp", &raw const exception),
        ] {
            let path = dir.path().join(name);
            let error =
                write_dump(&path, HANDLE::default(), NO_SUCH_PROCESS, pointers).unwrap_err();
            assert!(
                error
                    .to_string()
                    .starts_with("writing the minidump failed: "),
                "{error}"
            );
            assert!(path.is_file(), "{name} was created before the API failed");
        }
    }

    #[test]
    fn a_dump_file_that_cannot_be_created_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        // A folder is in the way of the file.
        let error = write_dump(
            dir.path(),
            HANDLE::default(),
            NO_SUCH_PROCESS,
            std::ptr::null(),
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(!text.starts_with("writing the minidump failed"), "{text}");
        assert!(text.contains(&dir.path().display().to_string()), "{text}");

        // The same through the dump of this process, whose file name is derived from the stamp.
        let taken = dir
            .path()
            .join(format!("crash-7-{}.dmp", std::process::id()));
        std::fs::create_dir(&taken).unwrap();
        let error = write_own_dump(dir.path(), 7, std::ptr::null()).unwrap_err();
        let text = error.to_string();
        assert!(!text.starts_with("writing the minidump failed"), "{text}");
        assert!(text.contains(&taken.display().to_string()), "{text}");
    }

    #[test]
    fn a_dump_folder_that_cannot_be_created_is_reported_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        let error = write_own_dump(&file.join("crashes"), 7, std::ptr::null()).unwrap_err();
        let text = error.to_string();
        assert!(!text.starts_with("writing the minidump failed"), "{text}");
        assert!(text.contains("a-file"), "{text}");
    }

    #[test]
    fn the_dump_of_this_process_is_named_after_the_stamp_and_the_process_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_own_dump(dir.path(), 1234, std::ptr::null()).unwrap();
        assert_eq!(
            path,
            dir.path()
                .join(format!("crash-1234-{}.dmp", std::process::id()))
        );
        assert!(std::fs::metadata(&path).unwrap().len() > 10_000);
    }
}
