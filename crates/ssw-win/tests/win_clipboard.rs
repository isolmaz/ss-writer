//! Clipboard round trips on this process.
//!
//! The clipboard is process-global state that belongs to the user, so every test runs inside a
//! [`ClipboardSession`]: it takes a process-wide lock (tests of this binary run in parallel),
//! copies every restorable format of the user's clipboard before the test touches it, and puts
//! them all back when it is dropped, also when the test panics. A clipboard that stays busy
//! (another process holds it) is retried with a short backoff; when it cannot be read the test
//! fails instead of overwriting contents it could not save.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ssw_ui::services::{Clipboard, ClipboardContent, ClipboardFormat};
use ssw_win::com::ComGuard;
use ssw_win::services::clipboard::{FORMAT_VSCODE_EDITOR_DATA, WindowsClipboard, decode_text};
use ssw_win::wic::{DecodedImage, Wic};
use ssw_win::window::MessageWindow;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::{
    CloseEnhMetaFile, CreateEnhMetaFileW, DeleteEnhMetaFile, GetEnhMetaFileBits, HENHMETAFILE,
    Rectangle, SetEnhMetaFileBits,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::core::{HSTRING, PCWSTR};

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_METAFILEPICT: u32 = 3;
const CF_OEMTEXT: u32 = 7;
const CF_PALETTE: u32 = 9;
const CF_UNICODETEXT: u32 = 13;
const CF_ENHMETAFILE: u32 = 14;

/// A registered format only this test binary uses.
const CUSTOM_FORMAT: &str = "ssw-win-test-custom-format";

/// How many times a busy clipboard is retried (about 1.3 s in total).
const OPEN_ATTEMPTS: u32 = 60;

/// Serializes the tests that touch the clipboard.
static EXCLUSIVE: Mutex<()> = Mutex::new(());

/// The clipboard, open on this thread; closed when dropped, so a panic never leaves it locked.
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    /// Opens the clipboard for `owner`, pausing a little longer after every failed attempt
    /// while another process (a clipboard manager, a remote session) holds it.
    fn open(owner: HWND) -> windows::core::Result<Self> {
        let mut attempt = 0;
        loop {
            // SAFETY: opens the clipboard for the calling thread; `Drop` closes it again.
            match unsafe { OpenClipboard(Some(owner)) } {
                Ok(()) => return Ok(Self),
                Err(error) if attempt + 1 >= OPEN_ATTEMPTS => return Err(error),
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(u64::from(attempt.min(20)) + 5));
            attempt += 1;
        }
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful `OpenClipboard` of `open`.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// What a format's data handle is, and therefore how it can be copied.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// Not copied (see [`classify`]).
    Skip,
    /// A global-memory block: copied as bytes.
    Memory,
    /// An enhanced metafile handle: copied as the metafile's bits.
    EnhMetafile,
}

/// Decides how `format` is saved, given every format the clipboard offers.
///
/// - `CF_TEXT` / `CF_OEMTEXT` next to `CF_UNICODETEXT` are synthesized by the system.
/// - `CF_BITMAP` is a GDI handle and `CF_PALETTE` belongs to it: the system derives the bitmap
///   from the `CF_DIB` / `CF_DIBV5` that are saved as memory. `CF_METAFILEPICT` embeds a
///   metafile handle and is derived from `CF_ENHMETAFILE`, which is saved as bits.
/// - The owner-display range (0x80..=0x8F), the private range (0x200..=0x2FF) and the GDI
///   object range (0x300..=0x3FF) hold handles that are not global memory.
///
/// Everything else (standard formats such as `CF_HDROP`, registered formats) is global memory.
fn classify(format: u32, offered: &[u32]) -> Kind {
    match format {
        CF_TEXT | CF_OEMTEXT if offered.contains(&CF_UNICODETEXT) => Kind::Skip,
        CF_BITMAP | CF_PALETTE | CF_METAFILEPICT => Kind::Skip,
        CF_ENHMETAFILE => Kind::EnhMetafile,
        0x80..=0x8F | 0x200..=0x3FF => Kind::Skip,
        _ => Kind::Memory,
    }
}

#[derive(Debug, PartialEq)]
enum Payload {
    Memory(Vec<u8>),
    EnhMetafile(Vec<u8>),
}

/// Every saved format, in the order the clipboard offered them (richest first).
#[derive(Debug, Default, PartialEq)]
struct Snapshot {
    entries: Vec<(u32, Payload)>,
}

impl Snapshot {
    /// The bytes saved for a global-memory format.
    fn memory(&self, format: u32) -> Option<&[u8]> {
        self.entries
            .iter()
            .find_map(|(saved, payload)| match payload {
                Payload::Memory(bytes) if *saved == format => Some(bytes.as_slice()),
                _ => None,
            })
    }

    /// The bits saved for the enhanced metafile.
    fn metafile(&self) -> Option<&[u8]> {
        self.entries.iter().find_map(|(_, payload)| match payload {
            Payload::EnhMetafile(bits) => Some(bits.as_slice()),
            _ => None,
        })
    }
}

/// Copies the bytes of a global-memory handle that belongs to the open clipboard.
fn read_memory(handle: HANDLE) -> Option<Vec<u8>> {
    let global = HGLOBAL(handle.0);
    // SAFETY: the clipboard is open, so the handle is valid; the copy stays within the block's
    // own size and the block is unlocked again right after.
    unsafe {
        let size = GlobalSize(global);
        let pointer = GlobalLock(global).cast::<u8>().cast_const();
        if pointer.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

/// Copies the bits of an enhanced metafile handle that belongs to the open clipboard.
fn read_metafile(handle: HANDLE) -> Option<Vec<u8>> {
    let metafile = HENHMETAFILE(handle.0);
    // SAFETY: the clipboard is open, so the metafile is valid; the buffer has the size the
    // first call reported.
    unsafe {
        let size = GetEnhMetaFileBits(metafile, None);
        if size == 0 {
            return None;
        }
        let mut bits = vec![0u8; size as usize];
        (GetEnhMetaFileBits(metafile, Some(&mut bits)) == size).then_some(bits)
    }
}

/// Saves the clipboard: opens it, copies every restorable format and closes it again.
fn capture(owner: HWND) -> windows::core::Result<Snapshot> {
    let _open = OpenClipboardGuard::open(owner)?;
    let mut offered = Vec::new();
    let mut format = 0;
    loop {
        // SAFETY: the clipboard is open.
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        offered.push(format);
    }
    let mut snapshot = Snapshot::default();
    for &format in &offered {
        let read: fn(HANDLE) -> Option<Payload> = match classify(format, &offered) {
            Kind::Skip => continue,
            Kind::Memory => |handle| read_memory(handle).map(Payload::Memory),
            Kind::EnhMetafile => |handle| read_metafile(handle).map(Payload::EnhMetafile),
        };
        // SAFETY: the clipboard is open; a failure (a format that cannot be rendered) skips it.
        let Ok(handle) = (unsafe { GetClipboardData(format) }) else {
            continue;
        };
        if let Some(payload) = read(handle) {
            snapshot.entries.push((format, payload));
        }
    }
    Ok(snapshot)
}

/// Gives a copy of `bytes` to the open clipboard as `format`; true on success.
fn put_memory(format: u32, bytes: &[u8]) -> bool {
    // SAFETY: the allocation is filled while locked; the clipboard owns the handle after a
    // successful `SetClipboardData`, otherwise it is freed here.
    unsafe {
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
            return false;
        };
        let pointer = GlobalLock(global).cast::<u8>();
        if pointer.is_null() {
            let _ = GlobalFree(Some(global));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        let _ = GlobalUnlock(global);
        if SetClipboardData(format, Some(HANDLE(global.0))).is_err() {
            let _ = GlobalFree(Some(global));
            return false;
        }
    }
    true
}

/// Gives an enhanced metafile built from `bits` to the open clipboard; true on success.
fn put_metafile(bits: &[u8]) -> bool {
    // SAFETY: the clipboard owns the metafile after a successful `SetClipboardData`,
    // otherwise it is deleted here.
    unsafe {
        let metafile = SetEnhMetaFileBits(bits);
        if metafile.is_invalid() {
            return false;
        }
        if SetClipboardData(CF_ENHMETAFILE, Some(HANDLE(metafile.0))).is_err() {
            let _ = DeleteEnhMetaFile(Some(metafile));
            return false;
        }
    }
    true
}

/// Replaces the clipboard contents with `snapshot`; returns the formats that were refused.
fn restore(owner: HWND, snapshot: &Snapshot) -> windows::core::Result<Vec<u32>> {
    let _open = OpenClipboardGuard::open(owner)?;
    // SAFETY: the clipboard is open.
    unsafe { EmptyClipboard() }?;
    let mut refused = Vec::new();
    for (format, payload) in &snapshot.entries {
        let stored = match payload {
            Payload::Memory(bytes) => put_memory(*format, bytes),
            Payload::EnhMetafile(bits) => put_metafile(bits),
        };
        if !stored {
            refused.push(*format);
        }
    }
    Ok(refused)
}

/// Exclusive use of the clipboard for one test; puts the user's contents back when dropped.
struct ClipboardSession {
    saved: Snapshot,
    owner: MessageWindow,
    /// Released last, so the next test starts only after the user's contents are back.
    _exclusive: MutexGuard<'static, ()>,
}

impl ClipboardSession {
    /// Waits for the other clipboard tests, then saves the user's clipboard. Panics (before
    /// anything was touched) when the clipboard cannot be read.
    fn begin() -> Self {
        let exclusive = EXCLUSIVE.lock().unwrap_or_else(PoisonError::into_inner);
        let owner = MessageWindow::new().unwrap();
        let saved = capture(owner.hwnd())
            .expect("the clipboard stayed busy: refusing to overwrite it without a backup");
        Self {
            saved,
            owner,
            _exclusive: exclusive,
        }
    }

    fn owner(&self) -> HWND {
        self.owner.hwnd()
    }
}

impl Drop for ClipboardSession {
    fn drop(&mut self) {
        let problem = match restore(self.owner.hwnd(), &self.saved) {
            Ok(refused) if refused.is_empty() => return,
            Ok(refused) => {
                format!("the clipboard refused the formats {refused:?} while being restored")
            }
            Err(error) => format!("the clipboard could not be reopened to restore it: {error}"),
        };
        if std::thread::panicking() {
            eprintln!("{problem}");
        } else {
            panic!("{problem}");
        }
    }
}

fn register(name: &str) -> u32 {
    // SAFETY: valid NUL-terminated name.
    unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) }
}

/// Puts a small enhanced metafile next to what is on the open clipboard; returns its bits.
fn add_metafile(owner: HWND) -> Vec<u8> {
    let _open = OpenClipboardGuard::open(owner).unwrap();
    // SAFETY: the device context is closed into a metafile that is measured and then handed to
    // the clipboard, which owns it after the successful `SetClipboardData`.
    unsafe {
        let device = CreateEnhMetaFileW(None, PCWSTR::null(), None, PCWSTR::null());
        let _ = Rectangle(device, 1, 2, 30, 20);
        let metafile = CloseEnhMetaFile(device);
        assert!(!metafile.is_invalid());
        let size = GetEnhMetaFileBits(metafile, None);
        let mut bits = vec![0u8; size as usize];
        assert_eq!(GetEnhMetaFileBits(metafile, Some(&mut bits)), size);
        SetClipboardData(CF_ENHMETAFILE, Some(HANDLE(metafile.0))).unwrap();
        bits
    }
}

#[test]
fn clipboard_round_trip_preserves_the_users_clipboard() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let session = ClipboardSession::begin();
    let wic = Wic::new().unwrap();

    let image = DecodedImage {
        width: 2,
        height: 1,
        bgra: vec![0, 0, 255, 255, 255, 0, 0, 255],
    };
    let png = wic.encode_png(&image).unwrap();
    let files = vec![
        PathBuf::from(r"C:\temp\a.md"),
        PathBuf::from(r"C:\temp\ç b.txt"),
    ];
    let vscode = br#"{"version":1,"mode":"markdown"}"#.to_vec();
    let content = ClipboardContent {
        text: Some("satır 1\nsatır 2 İ😀".to_owned()),
        html: Some("<p><b>merhaba</b></p>".to_owned()),
        rtf: Some(r"{\rtf1\ansi hello}".to_owned()),
        image: Some(png),
        files: files.clone(),
        custom: vec![(FORMAT_VSCODE_EDITOR_DATA.to_owned(), vscode.clone())],
    };

    let mut clipboard = WindowsClipboard::new(session.owner(), wic.clone());
    clipboard.set(content.clone()).unwrap();
    assert_eq!(
        clipboard.text().unwrap().as_deref(),
        content.text.as_deref()
    );
    assert_eq!(
        clipboard.html().unwrap().as_deref(),
        content.html.as_deref()
    );
    assert_eq!(clipboard.rtf().unwrap().as_deref(), content.rtf.as_deref());
    assert_eq!(clipboard.files().unwrap(), files);
    assert_eq!(
        clipboard.custom(FORMAT_VSCODE_EDITOR_DATA).unwrap(),
        Some(vscode.clone())
    );
    assert_eq!(
        clipboard.vscode_mode().unwrap().as_deref(),
        Some("markdown")
    );
    assert_eq!(clipboard.custom("ssw-no-such-format").unwrap(), None);
    let pasted = clipboard.image().unwrap().unwrap();
    let decoded = wic.decode(&pasted).unwrap();
    assert_eq!((decoded.width, decoded.height), (2, 1));
    assert_eq!(decoded.pixel(0, 0), Some([0, 0, 255, 255]));

    let formats = clipboard.formats().unwrap();
    for expected in [
        ClipboardFormat::Text,
        ClipboardFormat::Html,
        ClipboardFormat::Rtf,
        ClipboardFormat::Image,
        ClipboardFormat::Files,
        ClipboardFormat::Custom(FORMAT_VSCODE_EDITOR_DATA.to_owned()),
    ] {
        assert!(formats.contains(&expected), "{expected:?} in {formats:?}");
    }
    assert_eq!(
        formats
            .iter()
            .filter(|f| **f == ClipboardFormat::Image)
            .count(),
        1
    );

    // Text only: the other formats disappear.
    clipboard
        .set(ClipboardContent::from_text("only text"))
        .unwrap();
    assert_eq!(clipboard.html().unwrap(), None);
    assert_eq!(clipboard.image().unwrap(), None);
    assert!(clipboard.files().unwrap().is_empty());
    assert_eq!(clipboard.formats().unwrap(), vec![ClipboardFormat::Text]);

    clipboard.clear().unwrap();
    assert_eq!(clipboard.text().unwrap(), None);
    assert!(clipboard.formats().unwrap().is_empty());
}

#[test]
fn restoring_a_snapshot_brings_back_text_a_custom_format_and_a_metafile() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let session = ClipboardSession::begin();
    let owner = session.owner();
    let mut clipboard = WindowsClipboard::new(owner, Wic::new().unwrap());

    let text = "Çay İ 😀\nsatır 2";
    // Every byte value, NULs included, and an odd length: the format is opaque binary.
    let binary: Vec<u8> = (0..=255u8).chain([0, 0, 7]).collect();
    clipboard
        .set(ClipboardContent {
            text: Some(text.to_owned()),
            custom: vec![(CUSTOM_FORMAT.to_owned(), binary.clone())],
            ..ClipboardContent::default()
        })
        .unwrap();
    let metafile = add_metafile(owner);

    let saved = capture(owner).unwrap();
    let custom_id = register(CUSTOM_FORMAT);
    assert_eq!(
        saved.memory(CF_UNICODETEXT).map(decode_text).as_deref(),
        Some(text)
    );
    assert_eq!(saved.memory(custom_id), Some(binary.as_slice()));
    assert_eq!(saved.metafile(), Some(metafile.as_slice()));
    assert!(
        saved.memory(CF_TEXT).is_none() && saved.memory(CF_OEMTEXT).is_none(),
        "formats the system synthesizes from the Unicode text are not saved"
    );
    // The offering order (priority) is kept: text, then the custom format, then the metafile.
    let order: Vec<u32> = saved.entries.iter().map(|(format, _)| *format).collect();
    let at = |format: u32| order.iter().position(|&offered| offered == format).unwrap();
    assert!(
        at(CF_UNICODETEXT) < at(custom_id) && at(custom_id) < at(CF_ENHMETAFILE),
        "{order:?}"
    );

    // Wipe the clipboard, then bring everything back from the snapshot.
    clipboard.clear().unwrap();
    assert_eq!(clipboard.text().unwrap(), None);
    assert_eq!(clipboard.custom(CUSTOM_FORMAT).unwrap(), None);
    assert!(restore(owner, &saved).unwrap().is_empty());

    assert_eq!(clipboard.text().unwrap().as_deref(), Some(text));
    assert_eq!(clipboard.custom(CUSTOM_FORMAT).unwrap(), Some(binary));
    let again = capture(owner).unwrap();
    assert_eq!(
        again.metafile(),
        Some(metafile.as_slice()),
        "the metafile survives byte for byte"
    );
    assert_eq!(again.memory(CF_UNICODETEXT), saved.memory(CF_UNICODETEXT));
    // An empty snapshot (the user had nothing on the clipboard) restores to an empty clipboard.
    assert!(restore(owner, &Snapshot::default()).unwrap().is_empty());
    assert!(clipboard.formats().unwrap().is_empty());
}
