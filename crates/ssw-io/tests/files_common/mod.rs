//! Helpers shared by the `files_*` integration tests (included with `mod files_common;`).
//!
//! Tests that move files to the Recycle Bin name them with a token no other process can have
//! and purge exactly the Recycle Bin items carrying that token afterwards, so nothing of the
//! test run stays in the user's bin.

use std::ffi::c_void;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IBindCtx,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOLDERID_RecycleBinFolder,
    FileOperation, IEnumShellItems, IFileOperation, IFileOperationProgressSink, IShellItem,
    KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN_NORMALDISPLAY,
};
use windows::core::IUnknown;

/// A unique token for the names of items a test recycles. Purges the matching Recycle Bin
/// items on drop, also when the test fails halfway.
#[derive(Debug)]
pub struct RecycleBinGuard {
    token: String,
}

impl RecycleBinGuard {
    pub fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self {
            token: format!("ssw-io-test-{}-{nanos}", std::process::id()),
        }
    }

    /// A name containing the token, e.g. `name("-note.md")`.
    pub fn name(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.token)
    }

    /// Permanently deletes the Recycle Bin items whose name contains the token and returns how
    /// many there were.
    ///
    /// # Panics
    /// When the Recycle Bin cannot be enumerated or the items cannot be deleted.
    pub fn purge(&self) -> usize {
        match self.try_purge() {
            Ok(count) => count,
            Err(error) => panic!("purging the Recycle Bin items of this test failed: {error}"),
        }
    }

    fn try_purge(&self) -> Result<usize, String> {
        // COM state is per thread: use a short-lived single-threaded apartment of our own.
        // COM errors are not `Send`, so they cross the thread boundary as text.
        let token = self.token.clone();
        std::thread::spawn(move || purge_on_sta(&token).map_err(|error| error.to_string()))
            .join()
            .unwrap_or_else(|_| Err("the purge thread panicked".to_owned()))
    }
}

impl Drop for RecycleBinGuard {
    fn drop(&mut self) {
        // Best effort: a failing cleanup must not mask the test's own result.
        let _ = self.try_purge();
    }
}

fn purge_on_sta(token: &str) -> windows::core::Result<usize> {
    // SAFETY: initializes COM for this dedicated thread; balanced by `CoUninitialize` below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
    let result = purge_items(token);
    // SAFETY: balances the successful `CoInitializeEx` above.
    unsafe { CoUninitialize() };
    result
}

fn purge_items(token: &str) -> windows::core::Result<usize> {
    // SAFETY: `FOLDERID_RecycleBinFolder` is a valid GUID; no token means the current user.
    let bin: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) }?;
    // SAFETY: `bin` is a valid shell item on this thread; `BHID_EnumItems` yields an enumerator.
    let items: IEnumShellItems = unsafe { bin.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems) }?;
    let mut ours = Vec::new();
    loop {
        let mut slot: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        // SAFETY: `slot` has room for the one requested item and `fetched` is a valid out value.
        unsafe { items.Next(&mut slot, Some(&raw mut fetched)) }?;
        let Some(item) = slot[0].take() else {
            break;
        };
        // SAFETY: `item` is a valid shell item.
        let name = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }?;
        // SAFETY: on success the shell returns a NUL-terminated string that we own.
        let text = String::from_utf16_lossy(unsafe { name.as_wide() });
        // SAFETY: the display name was allocated by the shell and is released exactly once.
        unsafe { CoTaskMemFree(Some(name.0.cast::<c_void>().cast_const())) };
        if text.contains(token) {
            ours.push(item);
        }
    }
    if ours.is_empty() {
        return Ok(0);
    }
    // SAFETY: `FileOperation` is the registered CLSID of `IFileOperation`.
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None::<&IUnknown>, CLSCTX_ALL) }?;
    // No `FOF_ALLOWUNDO`: deleting from the bin is permanent. No UI of any kind.
    // SAFETY: valid COM object on its owning thread.
    unsafe { operation.SetOperationFlags(FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) }?;
    for item in &ours {
        // SAFETY: `item` is a valid shell item; no progress sink is used.
        unsafe { operation.DeleteItem(item, None::<&IFileOperationProgressSink>) }?;
    }
    // SAFETY: valid COM object on its owning thread.
    unsafe { operation.PerformOperations() }?;
    Ok(ours.len())
}
