//! Crash handling against the real Win32 APIs: minidumps of this very test process are written
//! into temp folders; no test crashes the process. The handlers are process-global, so every
//! test runs under one lock.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use windows::Win32::Foundation::{E_ACCESSDENIED, NTSTATUS};
use windows::Win32::System::Diagnostics::Debug::{
    CONTEXT, EXCEPTION_EXECUTE_HANDLER, EXCEPTION_POINTERS, EXCEPTION_RECORD, RtlCaptureContext,
};

use ssw_io::crash::{CrashError, install, unhandled_exception_filter, write_minidump};
use ssw_io::fs::FsError;

static GLOBAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    GLOBAL.lock().unwrap_or_else(PoisonError::into_inner)
}

const THREAD_LIST_STREAM: u32 = 3;
const MODULE_LIST_STREAM: u32 = 4;
const EXCEPTION_STREAM: u32 = 6;
const THREAD_INFO_LIST_STREAM: u32 = 17;

/// The stream types listed in the directory of a minidump.
fn stream_types(path: &Path) -> Vec<u32> {
    let mut file = std::fs::File::open(path).unwrap();
    let mut header = [0u8; 32];
    file.read_exact(&mut header).unwrap();
    assert_eq!(&header[..4], b"MDMP", "{path:?} is not a minidump");
    let count = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let directory_rva = u32::from_le_bytes(header[12..16].try_into().unwrap());
    file.seek(SeekFrom::Start(u64::from(directory_rva)))
        .unwrap();
    let mut directory = vec![0u8; count as usize * 12];
    file.read_exact(&mut directory).unwrap();
    directory
        .as_chunks::<12>()
        .0
        .iter()
        .map(|entry| u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]))
        .collect()
}

fn dumps(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "dmp"))
        .collect();
    found.sort();
    found
}

/// Records how often the emergency save ran and how many dumps already existed at that time.
#[derive(Default)]
struct Probe {
    calls: AtomicUsize,
    dumps_at_call: Mutex<Vec<usize>>,
}

impl Probe {
    fn emergency(self: &Arc<Self>, dir: &Path) -> Box<dyn Fn() + Send + Sync> {
        let probe = Arc::clone(self);
        let dir = dir.to_path_buf();
        Box::new(move || {
            probe
                .dumps_at_call
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(dumps(&dir).len());
            probe.calls.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn dumps_at_call(&self) -> Vec<usize> {
        self.dumps_at_call
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[test]
fn a_minidump_of_this_process_is_written_into_the_folder() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let crashes = dir.path().join("crashes").join("nested");
    let path = write_minidump(&crashes).unwrap();

    assert_eq!(path.parent().unwrap(), crashes);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("crash-"), "{name}");
    assert!(
        name.ends_with(&format!("-{}.dmp", std::process::id())),
        "{name}"
    );
    assert!(std::fs::metadata(&path).unwrap().len() > 10_000);
    assert_eq!(dumps(&crashes), std::slice::from_ref(&path));

    let streams = stream_types(&path);
    for expected in [
        THREAD_LIST_STREAM,
        MODULE_LIST_STREAM,
        THREAD_INFO_LIST_STREAM,
    ] {
        assert!(
            streams.contains(&expected),
            "stream {expected} in {streams:?}"
        );
    }
    assert!(
        !streams.contains(&EXCEPTION_STREAM),
        "no exception was reported"
    );
}

#[test]
fn dump_failures_are_classified() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    let error = write_minidump(&file).unwrap_err();
    assert!(
        matches!(
            error,
            CrashError::Fs(FsError::AlreadyExists(_) | FsError::Os { .. })
        ),
        "{error:?}"
    );
    assert!(error.to_string().contains("a-file"), "{error}");

    let dump_error = CrashError::from(windows::core::Error::from(E_ACCESSDENIED));
    assert!(matches!(dump_error, CrashError::Dump(_)));
    assert!(
        dump_error
            .to_string()
            .starts_with("writing the minidump failed: "),
        "{dump_error}"
    );
}

#[test]
fn the_exception_filter_dumps_first_then_runs_the_emergency_save() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let crashes = dir.path().join("crashes");
    let probe = Arc::new(Probe::default());
    install(crashes.clone(), probe.emergency(&crashes));

    // SAFETY: a null pointer is allowed: the filter then writes a dump without exception data.
    let verdict = unsafe { unhandled_exception_filter(std::ptr::null()) };
    assert_eq!(verdict, EXCEPTION_EXECUTE_HANDLER);
    assert_eq!(probe.calls(), 1);
    assert_eq!(
        probe.dumps_at_call(),
        [1],
        "the dump exists before the emergency save runs"
    );
    let written = dumps(&crashes);
    assert_eq!(written.len(), 1);
    assert!(!stream_types(&written[0]).contains(&EXCEPTION_STREAM));

    // The handler is ready for the next crash.
    std::thread::sleep(std::time::Duration::from_millis(5));
    // SAFETY: as above.
    unsafe { unhandled_exception_filter(std::ptr::null()) };
    assert_eq!(probe.calls(), 2);
}

#[test]
fn a_reported_exception_ends_up_in_the_dump() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let crashes = dir.path().join("crashes");
    let probe = Arc::new(Probe::default());
    install(crashes.clone(), probe.emergency(&crashes));

    // The `windows` crate declares `CONTEXT` with 8-byte alignment, but the OS stores XMM
    // registers with aligned moves: the buffer must be 16-byte aligned.
    #[repr(C, align(16))]
    struct AlignedContext(CONTEXT);
    let mut context = AlignedContext(CONTEXT::default());
    // SAFETY: `RtlCaptureContext` fills the `CONTEXT` of the calling thread; the buffer is a
    // 16-byte aligned local.
    unsafe { RtlCaptureContext(&mut context.0) };
    let mut record = EXCEPTION_RECORD {
        ExceptionCode: NTSTATUS(0x4000_0001),
        ..EXCEPTION_RECORD::default()
    };
    let pointers = EXCEPTION_POINTERS {
        ExceptionRecord: &mut record,
        ContextRecord: &mut context.0,
    };
    // SAFETY: both pointers refer to live locals of this thread for the whole call.
    let verdict = unsafe { unhandled_exception_filter(&pointers) };
    assert_eq!(verdict, EXCEPTION_EXECUTE_HANDLER);
    assert_eq!(probe.calls(), 1);
    let written = dumps(&crashes);
    assert_eq!(written.len(), 1);
    assert!(stream_types(&written[0]).contains(&EXCEPTION_STREAM));
}

#[test]
fn a_crash_inside_the_emergency_save_does_not_recurse() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let crashes = dir.path().join("crashes");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    install(crashes.clone(), move || {
        counter.fetch_add(1, Ordering::SeqCst);
        // SAFETY: a null pointer is allowed; this simulates a second fault while handling one.
        let nested = unsafe { unhandled_exception_filter(std::ptr::null()) };
        assert_eq!(nested, EXCEPTION_EXECUTE_HANDLER);
    });
    // SAFETY: a null pointer is allowed.
    unsafe { unhandled_exception_filter(std::ptr::null()) };
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the nested fault was ignored"
    );
    assert_eq!(dumps(&crashes).len(), 1);

    // The guard is released afterwards: a later crash is handled again.
    std::thread::sleep(std::time::Duration::from_millis(5));
    // SAFETY: a null pointer is allowed.
    unsafe { unhandled_exception_filter(std::ptr::null()) };
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// Runs `body` on a fresh thread that panics at the end, and waits for it.
fn panic_on_new_thread(body: impl FnOnce() + Send + 'static) {
    let outcome = std::thread::spawn(move || {
        body();
        panic!("simulated panic");
    })
    .join();
    assert!(outcome.is_err());
}

#[test]
fn only_panics_of_the_ui_thread_trigger_the_dump_and_emergency_save() {
    let _serial = serial();
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first");
    let second = dir.path().join("second");
    let first_probe = Arc::new(Probe::default());
    let second_probe = Arc::new(Probe::default());

    // The installing thread is the UI thread; its panic dumps and saves.
    {
        let probe = Arc::clone(&first_probe);
        let folder = first.clone();
        panic_on_new_thread(move || install(folder.clone(), probe.emergency(&folder)));
    }
    assert_eq!(first_probe.calls(), 1);
    assert_eq!(first_probe.dumps_at_call(), [1]);
    assert_eq!(dumps(&first).len(), 1);

    // A panic on any other thread only reaches the previous hook.
    panic_on_new_thread(|| {});
    assert_eq!(first_probe.calls(), 1);
    assert_eq!(dumps(&first).len(), 1);

    // Installing again replaces folder, callback and UI thread; the hook is not doubled.
    {
        let probe = Arc::clone(&second_probe);
        let folder = second.clone();
        panic_on_new_thread(move || install(folder.clone(), probe.emergency(&folder)));
    }
    assert_eq!(
        second_probe.calls(),
        1,
        "one panic runs the emergency save once"
    );
    assert_eq!(dumps(&second).len(), 1);
    assert_eq!(first_probe.calls(), 1, "the replaced callback is gone");
    assert_eq!(dumps(&first).len(), 1);

    // The first UI thread no longer counts.
    panic_on_new_thread(|| {});
    assert_eq!(second_probe.calls(), 1);
}

/// This test never calls `install`: when it runs first in its process (always, under nextest)
/// the filter finds no handler configured and has nothing to dump or save. The verdict is the
/// same when another test's handler is installed already, so it holds in a shared process too.
#[test]
fn the_exception_filter_without_a_handler_still_ends_the_process_quietly() {
    let _serial = serial();
    // SAFETY: a null pointer is allowed.
    let verdict = unsafe { unhandled_exception_filter(std::ptr::null()) };
    assert_eq!(verdict, EXCEPTION_EXECUTE_HANDLER);
    // SAFETY: as above; the recursion guard was released by the first call.
    let again = unsafe { unhandled_exception_filter(std::ptr::null()) };
    assert_eq!(again, EXCEPTION_EXECUTE_HANDLER);
}
