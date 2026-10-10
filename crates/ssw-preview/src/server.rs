//! The DLL's life as a COM server: how many objects and locks keep it loaded
//! (`DllCanUnloadNow`) and the module handle window classes are registered with.

use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Foundation::{HINSTANCE, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleHandleExW,
};
use windows::core::PCWSTR;

/// COM objects of this DLL that are alive (class factories and preview handlers).
static LIVE_OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// Outstanding `IClassFactory::LockServer(TRUE)` calls.
static SERVER_LOCKS: AtomicUsize = AtomicUsize::new(0);

/// Proof that one COM object of this DLL is alive; the DLL stays loaded until every token is
/// dropped. Embed one in every COM object, so the count follows the object's real lifetime
/// (its last `Release`), however many interface pointers were handed out.
#[derive(Debug)]
pub(crate) struct ObjectToken(());

impl ObjectToken {
    /// Registers a new object.
    pub(crate) fn new() -> Self {
        LIVE_OBJECTS.fetch_add(1, Ordering::AcqRel);
        Self(())
    }
}

impl Drop for ObjectToken {
    fn drop(&mut self) {
        LIVE_OBJECTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// How many COM objects of this DLL are alive.
#[must_use]
pub fn live_objects() -> usize {
    LIVE_OBJECTS.load(Ordering::Acquire)
}

/// How many `LockServer(TRUE)` calls have not been balanced yet.
#[must_use]
pub fn server_locks() -> usize {
    SERVER_LOCKS.load(Ordering::Acquire)
}

/// `LockServer`: `true` takes a lock, `false` releases one. An unbalanced release is ignored.
pub(crate) fn lock_server(lock: bool) {
    if lock {
        SERVER_LOCKS.fetch_add(1, Ordering::AcqRel);
    } else {
        let _ = SERVER_LOCKS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |locks| {
            locks.checked_sub(1)
        });
    }
}

/// True when no object and no lock keeps the DLL in memory (`DllCanUnloadNow` answers `S_OK`).
#[must_use]
pub fn can_unload() -> bool {
    live_objects() == 0 && server_locks() == 0
}

/// The handle of the module this code is in (the DLL, or the test executable when linked as a
/// library), without taking a reference on it.
pub(crate) fn module_handle() -> HINSTANCE {
    let mut module = HMODULE::default();
    // SAFETY: with `FROM_ADDRESS` the "name" is an address inside the module; `module_handle`
    // is code of this very module. `UNCHANGED_REFCOUNT` means no reference has to be released.
    // The lookup cannot fail for an address of our own code; if it did, `module` stays null,
    // which `RegisterClassExW` / `CreateWindowExW` read as "the calling process's executable".
    let _ = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(module_handle as *const () as *const u16),
            &mut module,
        )
    };
    HINSTANCE(module.0)
}
