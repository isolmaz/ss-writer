//! The rig of the preview tests: the DLL's own `DllGetClassObject`, a hidden never-shown host
//! window, streams (memory, failing, size-less) and pixel helpers.
//!
//! Every test that creates COM objects of the DLL takes [`serial`] (a [`Session`] does): the
//! object and lock counts behind `DllCanUnloadNow` are process-wide.

// Every test crate uses a different subset of the helpers.
#![allow(dead_code)]

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

use ssw_preview::factory::{CLSID_PREVIEW_HANDLER, DllGetClassObject};
use ssw_preview::handler::render_preview;
use ssw_win::com::ComGuard;
use ssw_win::wic::DecodedImage;
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, HWND, RECT};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, IClassFactory, ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE,
    STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STREAM_SEEK,
};
use windows::Win32::System::Ole::IOleWindow;
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandlerVisuals, SHCreateMemStream};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HRESULT, Interface, PWSTR, Ref, implement, w};

static SERIAL: Mutex<()> = Mutex::new(());

/// Exclusive use of the DLL's process-wide counters; poisoned by a failed test is fine.
pub fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The class factory the DLL's `DllGetClassObject` hands out.
pub fn class_factory() -> IClassFactory {
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: valid GUID pointers and a valid out pointer.
    let status =
        unsafe { DllGetClassObject(&CLSID_PREVIEW_HANDLER, &IClassFactory::IID, &mut raw) };
    assert!(status.is_ok(), "DllGetClassObject failed: {status:?}");
    // SAFETY: on success `raw` is an owned `IClassFactory` pointer.
    unsafe { IClassFactory::from_raw(raw) }
}

/// A new handler from a new factory.
pub fn new_handler() -> IPreviewHandler {
    let factory = class_factory();
    // SAFETY: plain call on a live factory; no aggregation.
    unsafe { factory.CreateInstance::<_, IPreviewHandler>(None::<&windows::core::IUnknown>) }
        .expect("the factory creates a handler")
}

/// A hidden, never shown top-level window: the host of the preview.
#[derive(Debug)]
pub struct Parent(pub HWND);

impl Parent {
    pub fn new() -> Self {
        // SAFETY: creates a plain system "STATIC" window without `WS_VISIBLE`; it is never
        // shown or activated.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("ssw-preview test host"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                800,
                600,
                None,
                None,
                None,
                None,
            )
        }
        .expect("a hidden host window");
        Self(hwnd)
    }
}

impl Drop for Parent {
    fn drop(&mut self) {
        // SAFETY: destroys the window created in `new` on its own thread.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// Pixels of the pane the tests render.
pub const WIDTH: u32 = 600;
/// Height in pixels of the pane the tests render.
pub const HEIGHT: u32 = 400;
/// The pane's rectangle inside the host.
pub const RECT_PANE: RECT = RECT {
    left: 0,
    top: 0,
    right: WIDTH as i32,
    bottom: HEIGHT as i32,
};

/// `COLORREF` white.
pub const WHITE: u32 = 0x00FF_FFFF;
/// `COLORREF` black.
pub const BLACK: u32 = 0;

/// A handler in its host, driven the way Explorer drives it.
///
/// Field order is drop order: the handler (and its window) goes before the host window, the
/// host window before COM, COM before the lock.
#[derive(Debug)]
pub struct Session {
    pub handler: IPreviewHandler,
    pub parent: Parent,
    _com: ComGuard,
    _serial: MutexGuard<'static, ()>,
}

impl Session {
    /// A handler that has not been told anything yet.
    pub fn new() -> Self {
        let serial = serial();
        let com = ComGuard::apartment_threaded().expect("COM apartment");
        Self {
            handler: new_handler(),
            parent: Parent::new(),
            _com: com,
            _serial: serial,
        }
    }

    /// `Initialize` with a memory stream of `bytes`.
    pub fn initialize(&self, bytes: &[u8]) -> windows::core::Result<()> {
        self.initialize_with(&memory_stream(bytes))
    }

    /// `Initialize` with `stream`.
    pub fn initialize_with(&self, stream: &IStream) -> windows::core::Result<()> {
        let init: IInitializeWithStream = self.handler.cast().expect("IInitializeWithStream");
        // SAFETY: plain call with a live stream.
        unsafe { init.Initialize(stream, 0) }
    }

    /// `SetWindow` with the host and the pane rectangle.
    pub fn set_window(&self) -> windows::core::Result<()> {
        // SAFETY: valid window handle and rectangle.
        unsafe { self.handler.SetWindow(self.parent.0, &RECT_PANE) }
    }

    /// `DoPreview`.
    pub fn do_preview(&self) -> windows::core::Result<()> {
        // SAFETY: plain call.
        unsafe { self.handler.DoPreview() }
    }

    /// `SetBackgroundColor` and `SetTextColor`.
    pub fn colors(&self, background: u32, text: u32) {
        let visuals: IPreviewHandlerVisuals = self.handler.cast().expect("visuals");
        // SAFETY: plain calls.
        unsafe {
            visuals
                .SetBackgroundColor(windows::Win32::Foundation::COLORREF(background))
                .expect("background");
            visuals
                .SetTextColor(windows::Win32::Foundation::COLORREF(text))
                .expect("text color");
        }
    }

    /// A previewed document on white with black text: the whole protocol, in Explorer's order.
    pub fn show(markdown: &str) -> Self {
        let session = Self::new();
        session.colors(WHITE, BLACK);
        session.initialize(markdown.as_bytes()).expect("Initialize");
        session.set_window().expect("SetWindow");
        session.do_preview().expect("DoPreview");
        session
    }

    /// The child window of the preview.
    pub fn child(&self) -> windows::core::Result<HWND> {
        let window: IOleWindow = self.handler.cast().expect("IOleWindow");
        // SAFETY: plain call.
        unsafe { window.GetWindow() }
    }

    /// The pane's pixels (see `render_preview`).
    pub fn render(&self) -> DecodedImage {
        render_preview(&self.handler, WIDTH, HEIGHT, 96).expect("render the preview")
    }
}

/// A memory stream holding `bytes`.
pub fn memory_stream(bytes: &[u8]) -> IStream {
    // SAFETY: `bytes` is a valid slice; the stream copies it.
    unsafe { SHCreateMemStream(Some(bytes)) }.expect("a memory stream")
}

/// How a [`ScriptedStream`] misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `Read` fails.
    FailRead,
    /// `Stat` fails and `Read` hands out the given bytes, then ends.
    NoSize,
    /// `Stat` fails and `Read` never ends.
    Endless,
    /// A healthy stream whose `Stat` reports the file name and size, like the shell's file
    /// streams.
    Named(&'static str),
}

/// A stream that fails in a chosen way.
#[implement(IStream)]
#[derive(Debug)]
pub struct ScriptedStream {
    fault: Fault,
    data: RefCell<Vec<u8>>,
}

/// A [`ScriptedStream`] as an `IStream`.
pub fn scripted_stream(fault: Fault, data: &[u8]) -> IStream {
    ScriptedStream {
        fault,
        data: RefCell::new(data.to_vec()),
    }
    .into()
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for ScriptedStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        let count = match self.fault {
            Fault::FailRead => return E_FAIL,
            Fault::NoSize | Fault::Named(_) => {
                let mut data = self.data.borrow_mut();
                let count = data.len().min(cb as usize);
                // SAFETY: `pv` is valid for `cb >= count` bytes and `data` holds `count` bytes.
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), pv.cast::<u8>(), count) };
                data.drain(..count);
                count
            }
            Fault::Endless => {
                // SAFETY: `pv` is valid for `cb` bytes.
                unsafe { std::ptr::write_bytes(pv.cast::<u8>(), b'a', cb as usize) };
                cb as usize
            }
        };
        // SAFETY: `pcbread` is a valid out pointer (the caller's contract).
        unsafe { *pcbread = count as u32 };
        HRESULT(0)
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        E_NOTIMPL
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for ScriptedStream_Impl {
    fn Seek(
        &self,
        _dlibmove: i64,
        _dworigin: STREAM_SEEK,
        _plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(
        &self,
        _pstm: Ref<IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: u32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        let Fault::Named(name) = self.fault else {
            // Streams that cannot report a size: the reader must go by what `Read` yields.
            return Err(E_FAIL.into());
        };
        let mut stat = STATSTG {
            cbSize: self.data.borrow().len() as u64,
            ..Default::default()
        };
        if *grfstatflag != STATFLAG_NONAME {
            let units: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: the buffer holds `units.len()` code units; the caller frees the name
            // with `CoTaskMemFree`, as `STATFLAG_DEFAULT` requires.
            let buffer = unsafe { CoTaskMemAlloc(units.len() * size_of::<u16>()) }.cast::<u16>();
            assert!(!buffer.is_null(), "the task allocator is out of memory");
            // SAFETY: `buffer` is valid for `units.len()` units and cannot overlap `units`.
            unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len()) };
            stat.pwcsName = PWSTR(buffer);
        }
        // SAFETY: `pstatstg` is a valid out pointer (the caller's contract).
        unsafe { *pstatstg = stat };
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// `[b, g, r, a]` of a pixel that must exist.
pub fn pixel(image: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
    image
        .pixel(x, y)
        .unwrap_or_else(|| panic!("pixel ({x}, {y}) is outside the image"))
}

/// True when two pixels differ by more than `tolerance` in any channel.
pub fn differs(a: [u8; 4], b: [u8; 4], tolerance: u8) -> bool {
    a.iter().zip(&b).any(|(a, b)| a.abs_diff(*b) > tolerance)
}

/// How many pixels differ from `background`.
pub fn ink(image: &DecodedImage, background: [u8; 4]) -> usize {
    (0..image.height)
        .flat_map(|y| (0..image.width).map(move |x| (x, y)))
        .filter(|&(x, y)| differs(pixel(image, x, y), background, 24))
        .count()
}

/// The rows (top to bottom) that hold at least one pixel differing from `background`.
pub fn ink_rows(image: &DecodedImage, background: [u8; 4]) -> Vec<u32> {
    (0..image.height)
        .filter(|&y| (0..image.width).any(|x| differs(pixel(image, x, y), background, 24)))
        .collect()
}

/// The leftmost column that holds a pixel differing from `background`.
pub fn first_ink_column(image: &DecodedImage, background: [u8; 4]) -> Option<u32> {
    (0..image.width)
        .find(|&x| (0..image.height).any(|y| differs(pixel(image, x, y), background, 24)))
}
