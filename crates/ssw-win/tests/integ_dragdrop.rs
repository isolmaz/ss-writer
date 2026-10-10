//! OLE drag and drop, in-process: the drop target answers real `IDropTarget` calls
//! (`DragEnter` / `DragOver` / `DragLeave` / `Drop` with key states) over real data objects, the
//! data object of a drag out answers `IDataObject` calls, the drop source answers its button /
//! Escape table, and `start_drag` runs a whole drag through a loop that drives the target.
//! `DoDragDrop` (modal, pointer-driven) is never called.

mod support;

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::ptr::null_mut;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use ssw_win::com::{ComGuard, WinError};
use ssw_win::dragdrop::{
    AllowedEffects, DragButton, DragContent, DragEvent, DragKeys, DragLoop, DragProgress,
    DropHandler, DropKinds, Effect, OleGuard, WindowDropTarget, choose_effect, data_object,
    drag_progress, drop_source, drop_target, files_share_volume, start_drag,
};
use ssw_win::services::clipboard::{FORMAT_HTML, build_cf_html, build_hdrop, encode_text};
use support::MachineLock;
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_E_ALREADYREGISTERED, DRAGDROP_E_NOTREGISTERED,
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_DVASPECT,
    DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_NOTIMPL, E_POINTER, E_UNEXPECTED, HGLOBAL, HWND,
    OLE_E_ADVISENOTSUPPORTED, POINT, POINTL, S_OK,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
use windows::Win32::System::Com::StructuredStorage::CreateStreamOnHGlobal;
use windows::Win32::System::Com::{
    DATADIR_GET, DATADIR_SET, DVASPECT_CONTENT, DVASPECT_ICON, FORMATETC, IAdviseSink, IDataObject,
    IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
    TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK,
    DROPEFFECT_MOVE, DROPEFFECT_NONE, DROPEFFECT_SCROLL, IDropSource, IDropTarget,
    RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, HRESULT, HSTRING, implement, w};

/// `GetDriveTypeW` answer of a local fixed disk.
const DRIVE_FIXED: u32 = 3;

/// OLE initialized on the test's thread while holding the machine-wide drag lock (the shell's
/// drag helper keeps drag state, and real drags never overlap: there is one pointer), also
/// across the processes of a parallel test run. Bind it first in a test: it is then dropped
/// after every COM object of the test (OLE goes first, then the lock).
struct OleSession {
    _ole: OleGuard,
    _lock: MachineLock,
}

fn ole() -> OleSession {
    let lock = MachineLock::acquire("dragdrop");
    OleSession {
        _ole: OleGuard::initialize().unwrap(),
        _lock: lock,
    }
}

/// A temporary folder with two files; removed on drop.
struct Workspace {
    dir: PathBuf,
    files: Vec<PathBuf>,
}

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ssw-win-dragdrop-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let files = ["a.md", "b.md"]
            .map(|name| {
                let path = dir.join(name);
                std::fs::write(&path, name).unwrap();
                path
            })
            .to_vec();
        Self { dir, files }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A top-level window that is never shown: it only gives the drop target a client area.
struct HiddenWindow(HWND);

impl HiddenWindow {
    fn new() -> Self {
        // SAFETY: creates a window of the system `STATIC` class without `WS_VISIBLE`.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!("ssw-win drag and drop test"),
                WS_POPUP,
                120,
                80,
                200,
                200,
                None,
                None,
                None,
                None,
            )
        }
        .unwrap();
        Self(hwnd)
    }

    fn hwnd(&self) -> HWND {
        self.0
    }

    /// The screen point of the client point (`x`, `y`).
    fn screen(&self, x: i32, y: i32) -> POINTL {
        let mut point = POINT { x, y };
        // SAFETY: `point` is a valid in / out point and the window is alive.
        assert!(unsafe { ClientToScreen(self.0, &mut point) }.as_bool());
        POINTL {
            x: point.x,
            y: point.y,
        }
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window created in `new` on its own thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

/// What a handler callback saw.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    x: i32,
    y: i32,
    keys: DragKeys,
    allowed: AllowedEffects,
    kinds: DropKinds,
    files: Vec<PathBuf>,
}

impl Seen {
    fn of(event: &DragEvent<'_>) -> Self {
        Self {
            x: event.x,
            y: event.y,
            keys: event.keys,
            allowed: event.allowed,
            kinds: event.kinds,
            files: event.files.to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Call {
    Enter(Seen),
    Over(Seen),
    Leave,
    Dropped(Seen, DragContent),
}

/// Records every callback and answers with `answer`.
struct Recorder {
    calls: RefCell<Vec<Call>>,
    answer: Box<dyn Fn(&DragEvent<'_>) -> Effect>,
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }
}

impl DropHandler for Recorder {
    fn drag_enter(&self, event: &DragEvent<'_>) -> Effect {
        self.calls.borrow_mut().push(Call::Enter(Seen::of(event)));
        (self.answer)(event)
    }

    fn drag_over(&self, event: &DragEvent<'_>) -> Effect {
        self.calls.borrow_mut().push(Call::Over(Seen::of(event)));
        (self.answer)(event)
    }

    fn drag_leave(&self) {
        self.calls.borrow_mut().push(Call::Leave);
    }

    fn dropped(&self, event: &DragEvent<'_>, content: &DragContent) -> Effect {
        self.calls
            .borrow_mut()
            .push(Call::Dropped(Seen::of(event), content.clone()));
        (self.answer)(event)
    }
}

fn recorder(answer: impl Fn(&DragEvent<'_>) -> Effect + 'static) -> Rc<Recorder> {
    Rc::new(Recorder {
        calls: RefCell::new(Vec::new()),
        answer: Box::new(answer),
    })
}

fn enter(
    target: &IDropTarget,
    data: &IDataObject,
    keys: DragKeys,
    at: POINTL,
    allowed: AllowedEffects,
) -> windows::core::Result<DROPEFFECT> {
    let mut effect = allowed.to_ole();
    // SAFETY: `data` and `effect` outlive the call; the point is passed by value.
    unsafe { target.DragEnter(data, keys.to_ole(), at, &mut effect) }?;
    Ok(effect)
}

fn over(
    target: &IDropTarget,
    keys: DragKeys,
    at: POINTL,
    allowed: AllowedEffects,
) -> windows::core::Result<DROPEFFECT> {
    let mut effect = allowed.to_ole();
    // SAFETY: `effect` outlives the call; the point is passed by value.
    unsafe { target.DragOver(keys.to_ole(), at, &mut effect) }?;
    Ok(effect)
}

fn leave(target: &IDropTarget) -> windows::core::Result<()> {
    // SAFETY: a plain call on a live drop target.
    unsafe { target.DragLeave() }
}

fn drop_on(
    target: &IDropTarget,
    data: &IDataObject,
    keys: DragKeys,
    at: POINTL,
    allowed: AllowedEffects,
) -> windows::core::Result<DROPEFFECT> {
    let mut effect = allowed.to_ole();
    // SAFETY: `data` and `effect` outlive the call; the point is passed by value.
    unsafe { target.Drop(data, keys.to_ole(), at, &mut effect) }?;
    Ok(effect)
}

fn keys(shift: bool, ctrl: bool, alt: bool) -> DragKeys {
    DragKeys {
        shift,
        ctrl,
        alt,
        ..DragKeys::default()
    }
}

fn only(copy: bool, moves: bool, link: bool) -> AllowedEffects {
    AllowedEffects { copy, moves, link }
}

/// The error code of a COM result (`S_OK` for success).
fn code<T>(result: windows::core::Result<T>) -> HRESULT {
    match result {
        Ok(_) => S_OK,
        Err(error) => error.code(),
    }
}

fn files_content(work: &Workspace) -> DragContent {
    DragContent {
        files: work.files.clone(),
        ..DragContent::default()
    }
}

fn html_format() -> u16 {
    // SAFETY: a valid NUL-terminated name; the registered id fits 16 bits.
    unsafe { RegisterClipboardFormatW(&HSTRING::from(FORMAT_HTML)) as u16 }
}

fn request(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// The bytes of `format` served by `data` as global memory.
fn raw(data: &IDataObject, format: u16) -> Vec<u8> {
    let wanted = request(format);
    // SAFETY: `wanted` is a valid FORMATETC for the call.
    let mut medium = unsafe { data.GetData(&wanted) }.unwrap();
    assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);
    assert!(medium.pUnkForRelease.is_none());
    // SAFETY: `tymed` says the union holds an `HGLOBAL`, which stays valid until released.
    let global: HGLOBAL = unsafe { medium.u.hGlobal };
    // SAFETY: the block is locked only while its bytes are copied, then released once.
    unsafe {
        let size = GlobalSize(global);
        let pointer = GlobalLock(global).cast::<u8>();
        assert!(!pointer.is_null());
        let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
        let _ = GlobalUnlock(global);
        ReleaseStgMedium(&mut medium);
        bytes
    }
}

/// The `cfFormat` of every format `list` still enumerates.
fn drain(list: &IEnumFORMATETC) -> Vec<u16> {
    let mut formats = Vec::new();
    loop {
        let mut one = [FORMATETC::default()];
        let mut fetched = 0u32;
        // SAFETY: `one` and `fetched` are valid out values for one element.
        let result = unsafe { list.Next(&mut one, Some(&raw mut fetched)) };
        if result != S_OK {
            return formats;
        }
        assert_eq!(fetched, 1);
        assert_eq!(one[0].dwAspect, DVASPECT_CONTENT.0);
        assert_eq!(one[0].lindex, -1);
        assert_eq!(one[0].tymed, TYMED_HGLOBAL.0 as u32);
        assert!(one[0].ptd.is_null());
        formats.push(one[0].cfFormat);
    }
}

/// A data object of another source: it offers nothing, or offers the formats the app reads but
/// serves them as streams. Every other format (the ones the shell's drag helper probes for) is
/// refused.
#[implement(IDataObject)]
struct Foreign {
    formats: Vec<u16>,
}

impl Foreign_Impl {
    /// Whether `wanted` names a format this object offers.
    fn offers(&self, wanted: *const FORMATETC) -> bool {
        // SAFETY: COM callers pass a valid FORMATETC or null.
        unsafe { wanted.as_ref() }.is_some_and(|wanted| self.formats.contains(&wanted.cfFormat))
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for Foreign_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        if !self.offers(pformatetcin) {
            return Err(DV_E_FORMATETC.into());
        }
        // SAFETY: a stream over a fresh growable block that the stream owns.
        let stream = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }?;
        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        if self.offers(pformatetc) {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatectin: *const FORMATETC,
        _pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        Err(E_NOTIMPL.into())
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: windows::core::Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(E_NOTIMPL.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}

fn foreign(streams: bool) -> IDataObject {
    let formats = if streams {
        vec![CF_HDROP.0, CF_UNICODETEXT.0, html_format()]
    } else {
        Vec::new()
    };
    Foreign { formats }.into()
}

// ---------------------------------------------------------------------------------------------
// Effect rules and flag conversions
// ---------------------------------------------------------------------------------------------

#[test]
fn effects_follow_the_windows_conventions() {
    use Effect as E;
    let all = AllowedEffects::ALL;
    // (keys, allowed, same volume, expected)
    let table = [
        (keys(false, false, false), all, true, E::Move),
        (keys(false, false, false), all, false, E::Copy),
        (keys(false, true, false), all, true, E::Copy),
        (keys(true, false, false), all, false, E::Move),
        (keys(true, true, false), all, false, E::Link),
        (keys(false, false, true), all, true, E::Link),
        (keys(true, false, true), all, true, E::Link),
        // A key that asks for an effect the source does not allow refuses the drop.
        (
            keys(true, false, false),
            only(true, false, true),
            true,
            E::None,
        ),
        (
            keys(false, true, false),
            only(false, true, false),
            false,
            E::None,
        ),
        (
            keys(false, false, true),
            only(true, true, false),
            false,
            E::None,
        ),
        // Without keys the default falls back to what is allowed.
        (
            keys(false, false, false),
            only(true, false, false),
            true,
            E::Copy,
        ),
        (
            keys(false, false, false),
            only(false, true, false),
            false,
            E::Move,
        ),
        (
            keys(false, false, false),
            only(false, false, true),
            true,
            E::Link,
        ),
        (
            keys(false, false, false),
            only(false, false, false),
            true,
            E::None,
        ),
    ];
    for (held, allowed, same_volume, expected) in table {
        assert_eq!(
            choose_effect(held, allowed, same_volume),
            expected,
            "{held:?} {allowed:?} same volume: {same_volume}"
        );
    }
}

#[test]
fn ole_flags_convert_both_ways() {
    for (effect, ole) in [
        (Effect::None, DROPEFFECT_NONE),
        (Effect::Copy, DROPEFFECT_COPY),
        (Effect::Move, DROPEFFECT_MOVE),
        (Effect::Link, DROPEFFECT_LINK),
    ] {
        assert_eq!(effect.to_ole(), ole);
        assert_eq!(Effect::from_ole(ole), effect);
    }
    // A set of effects resolves to move, then copy, then link; the scroll flag is ignored.
    assert_eq!(
        Effect::from_ole(DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK),
        Effect::Move
    );
    assert_eq!(
        Effect::from_ole(DROPEFFECT_COPY | DROPEFFECT_LINK | DROPEFFECT_SCROLL),
        Effect::Copy
    );
    assert_eq!(Effect::from_ole(DROPEFFECT_SCROLL), Effect::None);

    for bits in 0..8u32 {
        let allowed = only(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
        assert_eq!(allowed.to_ole().0, bits);
        assert_eq!(AllowedEffects::from_ole(DROPEFFECT(bits)), allowed);
        assert!(allowed.allows(Effect::None));
        assert_eq!(allowed.allows(Effect::Copy), bits & 1 != 0);
        assert_eq!(allowed.allows(Effect::Move), bits & 2 != 0);
        assert_eq!(allowed.allows(Effect::Link), bits & 4 != 0);
    }
    assert_eq!(AllowedEffects::ALL.to_ole().0, 7);
    assert_eq!(AllowedEffects::default().to_ole(), DROPEFFECT_NONE);

    for bits in 0..32u32 {
        let held = DragKeys {
            left_button: bits & 1 != 0,
            right_button: bits & 2 != 0,
            shift: bits & 4 != 0,
            ctrl: bits & 8 != 0,
            alt: bits & 16 != 0,
        };
        // MK_LBUTTON 1, MK_RBUTTON 2, MK_SHIFT 4, MK_CONTROL 8 and MK_ALT 0x20.
        let flags = (bits & 15) | if held.alt { 0x20 } else { 0 };
        assert_eq!(held.to_ole(), MODIFIERKEYS_FLAGS(flags));
        assert_eq!(DragKeys::from_ole(MODIFIERKEYS_FLAGS(flags)), held);
    }
}

#[test]
fn volumes_decide_between_moving_and_copying() {
    let work = Workspace::new();
    let subfolder = work.dir.join("sub");
    std::fs::create_dir(&subfolder).unwrap();
    assert!(files_share_volume(&work.files, &work.dir));
    assert!(files_share_volume(&work.files[..1], &subfolder));
    // Nothing to compare, nothing known.
    assert!(!files_share_volume(&[], &work.dir));

    // A drive letter that is not mounted has no volume; a local disk that is not the one of
    // the temporary folder is another volume.
    // SAFETY: plain queries of the logical drives.
    let mounted = unsafe { GetLogicalDrives() };
    let free = (b'A'..=b'Z')
        .rev()
        .find(|letter| mounted & (1 << (letter - b'A')) == 0)
        .expect("a drive letter is free");
    assert!(!files_share_volume(
        &work.files,
        &PathBuf::from(format!("{}:\\", free as char))
    ));
    for letter in b'A'..=b'Z' {
        let root = format!("{}:\\", letter as char);
        // SAFETY: a valid NUL-terminated root path.
        let fixed = mounted & (1 << (letter - b'A')) != 0
            && unsafe { GetDriveTypeW(&HSTRING::from(root.as_str())) } == DRIVE_FIXED;
        if fixed && !files_share_volume(&work.files, &PathBuf::from(&root)) {
            assert!(!files_share_volume(
                &[PathBuf::from(format!("{root}some\\file.md"))],
                &work.dir
            ));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The drop target
// ---------------------------------------------------------------------------------------------

#[test]
fn dropping_files_reports_client_positions_and_applies_the_windows_rules() {
    let _ole = ole();
    let work = Workspace::new();
    let window = HiddenWindow::new();
    let folder = work.dir.clone();
    let handler = recorder(move |event| event.effect_into_folder(&folder));
    let target = drop_target(window.hwnd(), handler.clone());
    let data = data_object(&files_content(&work)).unwrap();
    let all = AllowedEffects::ALL;
    let seen = |held: DragKeys, allowed: AllowedEffects, x: i32, y: i32| Seen {
        x,
        y,
        keys: held,
        allowed,
        kinds: DropKinds {
            files: true,
            text: false,
            html: false,
        },
        files: work.files.clone(),
    };

    // Same volume: moves by default.
    let none = DragKeys::default();
    assert_eq!(
        enter(&target, &data, none, window.screen(30, 50), all).unwrap(),
        DROPEFFECT_MOVE
    );
    assert_eq!(handler.calls(), [Call::Enter(seen(none, all, 30, 50))]);

    // (keys, allowed, expected effect)
    let table = [
        (keys(false, true, false), all, DROPEFFECT_COPY),
        (keys(true, false, false), all, DROPEFFECT_MOVE),
        (keys(true, true, false), all, DROPEFFECT_LINK),
        (keys(false, false, true), all, DROPEFFECT_LINK),
        (none, only(true, false, true), DROPEFFECT_COPY),
        (
            keys(true, false, false),
            only(true, false, true),
            DROPEFFECT_NONE,
        ),
    ];
    for (held, allowed, expected) in table {
        assert_eq!(
            over(&target, held, window.screen(40, 60), allowed).unwrap(),
            expected,
            "{held:?} {allowed:?}"
        );
    }
    let calls = handler.calls();
    assert_eq!(calls.len(), 1 + table.len());
    assert_eq!(
        calls[1],
        Call::Over(seen(keys(false, true, false), all, 40, 60))
    );

    // The drop reads the files again and reports them.
    let ctrl = keys(false, true, false);
    assert_eq!(
        drop_on(&target, &data, ctrl, window.screen(5, 6), all).unwrap(),
        DROPEFFECT_COPY
    );
    assert_eq!(
        handler.calls().last(),
        Some(&Call::Dropped(
            seen(ctrl, all, 5, 6),
            DragContent {
                files: work.files.clone(),
                text: None,
                html: None
            }
        ))
    );

    // The drag is over: nothing to move across any more, nothing to leave.
    assert_eq!(
        code(over(&target, none, window.screen(1, 1), all)),
        E_UNEXPECTED
    );
    let before = handler.calls().len();
    leave(&target).unwrap();
    assert_eq!(handler.calls().len(), before);
}

#[test]
fn files_for_another_volume_copy_unless_a_key_says_otherwise() {
    let _ole = ole();
    let work = Workspace::new();
    let window = HiddenWindow::new();
    // No volume at all (a drive letter that is not mounted): the drop copies.
    // SAFETY: a plain query of the logical drives.
    let mounted = unsafe { GetLogicalDrives() };
    let free = (b'A'..=b'Z')
        .rev()
        .find(|letter| mounted & (1 << (letter - b'A')) == 0)
        .expect("a drive letter is free");
    let elsewhere = PathBuf::from(format!("{}:\\", free as char));
    let handler = recorder(move |event| event.effect_into_folder(&elsewhere));
    let target = drop_target(window.hwnd(), handler);
    let data = data_object(&files_content(&work)).unwrap();
    let at = window.screen(10, 10);
    let all = AllowedEffects::ALL;
    assert_eq!(
        enter(&target, &data, DragKeys::default(), at, all).unwrap(),
        DROPEFFECT_COPY
    );
    assert_eq!(
        over(&target, keys(true, false, false), at, all).unwrap(),
        DROPEFFECT_MOVE
    );
    leave(&target).unwrap();
}

#[test]
fn text_and_html_drops_deliver_normalized_content() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|event| event.effect_into_document());
    let target = drop_target(window.hwnd(), handler.clone());
    let content = DragContent {
        files: Vec::new(),
        text: Some("line one\nline two".to_owned()),
        html: Some("<p>hi <b>there</b></p>".to_owned()),
    };
    let data = data_object(&content).unwrap();
    let kinds = DropKinds {
        files: false,
        text: true,
        html: true,
    };
    assert_eq!(DropKinds::of(&data), kinds);
    assert!(kinds.any());
    assert!(!DropKinds::default().any());
    let at = window.screen(20, 30);
    let all = AllowedEffects::ALL;

    // Copy by default, move with Shift; a source that only allows moving gets a move.
    assert_eq!(
        enter(&target, &data, DragKeys::default(), at, all).unwrap(),
        DROPEFFECT_COPY
    );
    assert_eq!(
        over(&target, keys(true, false, false), at, all).unwrap(),
        DROPEFFECT_MOVE
    );
    assert_eq!(
        over(&target, DragKeys::default(), at, only(false, true, false)).unwrap(),
        DROPEFFECT_MOVE
    );
    assert_eq!(
        drop_on(&target, &data, DragKeys::default(), at, all).unwrap(),
        DROPEFFECT_COPY
    );
    let calls = handler.calls();
    let Some(Call::Dropped(event, dropped)) = calls.last() else {
        panic!("the drop was not reported: {calls:?}");
    };
    assert_eq!(event.kinds, kinds);
    assert!(event.files.is_empty());
    assert_eq!(*dropped, content);
}

#[test]
fn a_drag_that_leaves_is_reported_once() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let target = drop_target(window.hwnd(), handler.clone());
    let data = data_object(&DragContent {
        text: Some("x".to_owned()),
        ..DragContent::default()
    })
    .unwrap();
    enter(
        &target,
        &data,
        DragKeys::default(),
        window.screen(0, 0),
        AllowedEffects::ALL,
    )
    .unwrap();
    leave(&target).unwrap();
    leave(&target).unwrap();
    let calls = handler.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(matches!(calls[0], Call::Enter(_)));
    assert_eq!(calls[1], Call::Leave);
}

#[test]
fn a_target_without_the_shells_drag_image_helper_still_answers() {
    // This thread is not in a single-threaded apartment (it never initialized COM, or it
    // implicitly joined the process's multi-threaded one while another test runs), so the
    // target is created without the shell's helper.
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let target = drop_target(window.hwnd(), handler.clone());
    let data = data_object(&DragContent {
        text: Some("x".to_owned()),
        ..DragContent::default()
    })
    .unwrap();
    let at = window.screen(0, 0);
    let none = DragKeys::default();
    let all = AllowedEffects::ALL;
    assert_eq!(
        enter(&target, &data, none, at, all).unwrap(),
        DROPEFFECT_COPY
    );
    assert_eq!(over(&target, none, at, all).unwrap(), DROPEFFECT_COPY);
    leave(&target).unwrap();
    enter(&target, &data, none, at, all).unwrap();
    assert_eq!(
        drop_on(&target, &data, none, at, all).unwrap(),
        DROPEFFECT_COPY
    );
    assert_eq!(handler.calls().len(), 5);
}

#[test]
fn answers_the_source_does_not_allow_become_refusals() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Move);
    let target = drop_target(window.hwnd(), handler);
    let data = data_object(&DragContent {
        text: Some("x".to_owned()),
        ..DragContent::default()
    })
    .unwrap();
    let at = window.screen(0, 0);
    let copy_only = only(true, false, false);
    let none = DragKeys::default();
    assert_eq!(
        enter(&target, &data, none, at, copy_only).unwrap(),
        DROPEFFECT_NONE
    );
    assert_eq!(over(&target, none, at, copy_only).unwrap(), DROPEFFECT_NONE);
    assert_eq!(
        over(&target, none, at, AllowedEffects::ALL).unwrap(),
        DROPEFFECT_MOVE
    );
    assert_eq!(
        drop_on(&target, &data, none, at, copy_only).unwrap(),
        DROPEFFECT_NONE
    );
}

#[test]
fn data_nobody_asked_for_is_refused_without_calling_the_handler() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let target = drop_target(window.hwnd(), handler.clone());
    let data = foreign(false);
    assert_eq!(DropKinds::of(&data), DropKinds::default());
    assert!(DragContent::from_data_object(&data).is_empty());
    let at = window.screen(0, 0);
    let none = DragKeys::default();
    let all = AllowedEffects::ALL;
    assert_eq!(
        enter(&target, &data, none, at, all).unwrap(),
        DROPEFFECT_NONE
    );
    assert_eq!(over(&target, none, at, all).unwrap(), DROPEFFECT_NONE);
    assert_eq!(
        drop_on(&target, &data, none, at, all).unwrap(),
        DROPEFFECT_NONE
    );
    enter(&target, &data, none, at, all).unwrap();
    leave(&target).unwrap();
    assert!(handler.calls().is_empty());
}

#[test]
fn formats_served_in_another_medium_are_offered_but_not_read() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let target = drop_target(window.hwnd(), handler.clone());
    let data = foreign(true);
    let everything = DropKinds {
        files: true,
        text: true,
        html: true,
    };
    assert_eq!(DropKinds::of(&data), everything);
    let at = window.screen(0, 0);
    let none = DragKeys::default();
    let all = AllowedEffects::ALL;
    assert_eq!(
        enter(&target, &data, none, at, all).unwrap(),
        DROPEFFECT_COPY
    );
    assert_eq!(
        drop_on(&target, &data, none, at, all).unwrap(),
        DROPEFFECT_COPY
    );
    let calls = handler.calls();
    let Some(Call::Dropped(event, content)) = calls.last() else {
        panic!("the drop was not reported: {calls:?}");
    };
    assert_eq!(event.kinds, everything);
    assert!(content.is_empty(), "{content:?}");
}

#[test]
fn bad_arguments_and_missing_sessions_are_errors() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let target = drop_target(window.hwnd(), handler.clone());
    let data = data_object(&DragContent {
        text: Some("x".to_owned()),
        ..DragContent::default()
    })
    .unwrap();
    let at = window.screen(0, 0);
    let none = DragKeys::default();
    let all = AllowedEffects::ALL;

    // No drag in progress.
    assert_eq!(code(over(&target, none, at, all)), E_UNEXPECTED);
    assert_eq!(code(drop_on(&target, &data, none, at, all)), E_UNEXPECTED);
    // Null data object or effect pointer.
    // SAFETY: the null arguments are the point of the test; the target must reject them.
    unsafe {
        assert_eq!(
            code(target.DragEnter(None::<&IDataObject>, none.to_ole(), at, &mut all.to_ole())),
            E_POINTER
        );
        assert_eq!(
            code(target.DragEnter(&data, none.to_ole(), at, null_mut())),
            E_POINTER
        );
        assert_eq!(
            code(target.DragOver(none.to_ole(), at, null_mut())),
            E_POINTER
        );
        assert_eq!(
            code(target.Drop(None::<&IDataObject>, none.to_ole(), at, &mut all.to_ole())),
            E_POINTER
        );
        assert_eq!(
            code(target.Drop(&data, none.to_ole(), at, null_mut())),
            E_POINTER
        );
    }
    assert!(handler.calls().is_empty());
}

#[test]
fn a_registered_window_stays_a_drop_target_until_the_registration_is_dropped() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let registration = WindowDropTarget::register(window.hwnd(), handler.clone()).unwrap();
    let other = drop_target(window.hwnd(), handler.clone());
    // SAFETY: plain OLE calls with a live window and drop target.
    let again = unsafe { RegisterDragDrop(window.hwnd(), &other) };
    assert_eq!(code(again), DRAGDROP_E_ALREADYREGISTERED);
    drop(registration);
    // SAFETY: as above.
    let revoked = unsafe { RevokeDragDrop(window.hwnd()) };
    assert_eq!(code(revoked), DRAGDROP_E_NOTREGISTERED);

    // It can be registered again, and an invalid window cannot be registered.
    drop(WindowDropTarget::register(window.hwnd(), handler.clone()).unwrap());
    let invalid = WindowDropTarget::register(HWND::default(), handler);
    assert!(
        matches!(
            invalid,
            Err(WinError::Os {
                context: "RegisterDragDrop",
                ..
            })
        ),
        "{invalid:?}"
    );
}

#[test]
fn ole_initializes_nested_and_refuses_a_multi_threaded_apartment() {
    let outer = OleGuard::initialize().unwrap();
    let inner = OleGuard::initialize().unwrap();
    drop(inner);
    drop(outer);
    std::thread::spawn(|| {
        let _com = ComGuard::multi_threaded().unwrap();
        let error = OleGuard::initialize().unwrap_err();
        assert!(
            matches!(
                error,
                WinError::Os {
                    context: "OleInitialize",
                    ..
                }
            ),
            "{error:?}"
        );
    })
    .join()
    .unwrap();
}

// ---------------------------------------------------------------------------------------------
// The data object of a drag out
// ---------------------------------------------------------------------------------------------

fn full_content(work: &Workspace) -> DragContent {
    DragContent {
        files: work.files.clone(),
        text: Some("first\nsecond".to_owned()),
        html: Some("<em>second</em>".to_owned()),
    }
}

#[test]
fn an_empty_drag_makes_no_data_object() {
    let _ole = ole();
    assert!(DragContent::default().is_empty());
    let error = data_object(&DragContent::default()).unwrap_err();
    assert!(matches!(error, WinError::Invalid(_)), "{error:?}");
}

#[test]
fn the_data_object_serves_every_format_as_global_memory() {
    let _ole = ole();
    let work = Workspace::new();
    let content = full_content(&work);
    assert!(!content.is_empty());
    let data = data_object(&content).unwrap();
    let html = html_format();

    // SAFETY: plain calls on a live data object.
    let list = unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) }.unwrap();
    assert_eq!(drain(&list), [CF_HDROP.0, CF_UNICODETEXT.0, html]);
    // The enumerator the shell made over our list can be restarted, skipped and cloned. The
    // shell's clone does not carry the position over (it starts at the first format), so the
    // test only relies on the clone being an independent enumerator of the same list.
    // SAFETY: plain calls on a live enumerator.
    unsafe {
        list.Reset().unwrap();
        list.Skip(1).unwrap();
        let copy = list.Clone().unwrap();
        assert_eq!(drain(&list), [CF_UNICODETEXT.0, html]);
        copy.Reset().unwrap();
        assert_eq!(drain(&copy), [CF_HDROP.0, CF_UNICODETEXT.0, html]);
    }

    assert_eq!(raw(&data, CF_HDROP.0), build_hdrop(&work.files));
    assert_eq!(raw(&data, CF_UNICODETEXT.0), encode_text("first\nsecond"));
    assert_eq!(raw(&data, html), build_cf_html("<em>second</em>"));
    for format in [CF_HDROP.0, CF_UNICODETEXT.0, html] {
        let wanted = request(format);
        // SAFETY: `wanted` is a valid FORMATETC for the call.
        assert_eq!(unsafe { data.QueryGetData(&wanted) }, S_OK);
    }
    // What a target reads back is what was offered (line breaks are normalized to `\n`).
    assert_eq!(DragContent::from_data_object(&data), content);

    // Content without files only offers what it has.
    let text = data_object(&DragContent {
        text: Some("t".to_owned()),
        ..DragContent::default()
    })
    .unwrap();
    // SAFETY: plain calls on a live data object.
    let list = unsafe { text.EnumFormatEtc(DATADIR_GET.0 as u32) }.unwrap();
    assert_eq!(drain(&list), [CF_UNICODETEXT.0]);
}

#[test]
fn the_data_object_rejects_what_it_cannot_serve() {
    let _ole = ole();
    let work = Workspace::new();
    let data = data_object(&full_content(&work)).unwrap();

    let other_format = request(CF_BITMAP.0);
    let icon = FORMATETC {
        dwAspect: DVASPECT_ICON.0,
        ..request(CF_UNICODETEXT.0)
    };
    let indexed = FORMATETC {
        lindex: 0,
        ..request(CF_UNICODETEXT.0)
    };
    let stream = FORMATETC {
        tymed: TYMED_ISTREAM.0 as u32,
        ..request(CF_UNICODETEXT.0)
    };
    for (wanted, expected) in [
        (other_format, DV_E_FORMATETC),
        (icon, DV_E_DVASPECT),
        (indexed, DV_E_LINDEX),
        (stream, DV_E_TYMED),
    ] {
        // SAFETY: `wanted` is a valid FORMATETC for the calls.
        unsafe {
            assert_eq!(data.QueryGetData(&wanted), expected);
            assert_eq!(code(data.GetData(&wanted)), expected);
        }
    }
    // SAFETY: the null pointer is the point of the test; the data object must reject it.
    unsafe {
        assert_eq!(data.QueryGetData(std::ptr::null()), E_POINTER);
        assert_eq!(code(data.GetData(std::ptr::null())), E_POINTER);
    }

    // Unsupported operations.
    let wanted = request(CF_UNICODETEXT.0);
    let mut medium = STGMEDIUM::default();
    // SAFETY: valid pointers for every call; none of them keeps them.
    unsafe {
        assert_eq!(code(data.GetDataHere(&wanted, &mut medium)), E_NOTIMPL);
        assert_eq!(code(data.SetData(&wanted, &medium, false)), E_NOTIMPL);
        assert_eq!(code(data.EnumFormatEtc(DATADIR_SET.0 as u32)), E_NOTIMPL);
        assert_eq!(
            code(data.DAdvise(&wanted, 0, None::<&IAdviseSink>)),
            OLE_E_ADVISENOTSUPPORTED
        );
        assert_eq!(code(data.DUnadvise(1)), OLE_E_ADVISENOTSUPPORTED);
        assert_eq!(code(data.EnumDAdvise()), OLE_E_ADVISENOTSUPPORTED);
    }
}

#[test]
fn the_canonical_format_is_the_requested_one_without_a_target_device() {
    let _ole = ole();
    let work = Workspace::new();
    let data = data_object(&full_content(&work)).unwrap();
    let wanted = request(CF_UNICODETEXT.0);
    let mut canonical = FORMATETC::default();
    // SAFETY: valid in / out FORMATETCs for the calls.
    unsafe {
        assert_eq!(
            data.GetCanonicalFormatEtc(&wanted, &mut canonical),
            DATA_S_SAMEFORMATETC
        );
        assert_eq!(canonical, wanted);
        assert_eq!(
            data.GetCanonicalFormatEtc(std::ptr::null(), &mut canonical),
            E_POINTER
        );
        assert_eq!(data.GetCanonicalFormatEtc(&wanted, null_mut()), E_POINTER);
    }
}

// ---------------------------------------------------------------------------------------------
// The drop source
// ---------------------------------------------------------------------------------------------

#[test]
fn the_drop_source_ends_the_drag_on_escape_release_or_the_other_button() {
    use DragButton::{Left, Right};
    use DragProgress as P;
    // (escape, left held, right held, started with, expected)
    let table = [
        (false, true, false, Left, P::Continue),
        (false, false, false, Left, P::Drop),
        (true, true, false, Left, P::Cancel),
        (true, false, false, Left, P::Cancel),
        (false, true, true, Left, P::Cancel),
        (false, false, true, Left, P::Cancel),
        (false, false, true, Right, P::Continue),
        (false, false, false, Right, P::Drop),
        (false, true, true, Right, P::Cancel),
        (true, false, true, Right, P::Cancel),
    ];
    for (escape, left_button, right_button, button, expected) in table {
        let held = DragKeys {
            left_button,
            right_button,
            ..DragKeys::default()
        };
        assert_eq!(
            drag_progress(escape, held, button),
            expected,
            "escape {escape} {held:?} {button:?}"
        );
        let source = drop_source(button);
        let hresult = match expected {
            P::Continue => S_OK,
            P::Drop => DRAGDROP_S_DROP,
            P::Cancel => DRAGDROP_S_CANCEL,
        };
        // SAFETY: a plain call on a live drop source.
        let answer = unsafe { source.QueryContinueDrag(escape, held.to_ole()) };
        assert_eq!(answer, hresult);
    }
}

#[test]
fn the_drop_source_leaves_the_cursors_to_the_system() {
    let source = drop_source(DragButton::Left);
    for effect in [
        DROPEFFECT_NONE,
        DROPEFFECT_COPY,
        DROPEFFECT_MOVE,
        DROPEFFECT_LINK,
        DROPEFFECT_COPY | DROPEFFECT_SCROLL,
        DROPEFFECT_MOVE | DROPEFFECT_LINK,
    ] {
        // SAFETY: a plain call on a live drop source.
        let answer = unsafe { source.GiveFeedback(effect) };
        assert_eq!(answer, DRAGDROP_S_USEDEFAULTCURSORS, "{effect:?}");
    }
}

// ---------------------------------------------------------------------------------------------
// A whole drag
// ---------------------------------------------------------------------------------------------

/// What `DoDragDrop` does without the pointer: asks the source after each scripted key state and
/// forwards the verdicts to a real drop target.
struct InProcessLoop<'a> {
    target: &'a IDropTarget,
    window: &'a HiddenWindow,
    script: Vec<(bool, DragKeys)>,
}

impl DragLoop for InProcessLoop<'_> {
    fn run(
        &mut self,
        data: &IDataObject,
        source: &IDropSource,
        allowed: AllowedEffects,
    ) -> Result<Effect, WinError> {
        let at = self.window.screen(10, 10);
        let pressed = DragKeys {
            left_button: true,
            ..DragKeys::default()
        };
        enter(self.target, data, pressed, at, allowed).unwrap();
        for &(escape, held) in &self.script {
            // SAFETY: a plain call on a live drop source.
            let verdict = unsafe { source.QueryContinueDrag(escape, held.to_ole()) };
            if verdict == DRAGDROP_S_CANCEL {
                leave(self.target).unwrap();
                return Ok(Effect::None);
            }
            if verdict == DRAGDROP_S_DROP {
                let effect = drop_on(self.target, data, held, at, allowed).unwrap();
                return Ok(Effect::from_ole(effect));
            }
            over(self.target, held, at, allowed).unwrap();
        }
        panic!("the script must end the drag");
    }
}

/// A loop that cannot run.
struct BrokenLoop {
    ran: bool,
}

impl DragLoop for BrokenLoop {
    fn run(
        &mut self,
        _data: &IDataObject,
        _source: &IDropSource,
        _allowed: AllowedEffects,
    ) -> Result<Effect, WinError> {
        self.ran = true;
        Err(WinError::Invalid("no pointer"))
    }
}

#[test]
fn a_dragged_selection_arrives_at_the_drop_target_through_the_whole_protocol() {
    let _ole = ole();
    let work = Workspace::new();
    let window = HiddenWindow::new();
    let handler = recorder(|event| event.effect_into_document());
    let target = drop_target(window.hwnd(), handler.clone());
    let content = full_content(&work);
    let pressed = DragKeys {
        left_button: true,
        ..DragKeys::default()
    };
    let pressed_ctrl = DragKeys {
        ctrl: true,
        ..pressed
    };
    let mut drag_loop = InProcessLoop {
        target: &target,
        window: &window,
        script: vec![
            (false, pressed),
            (false, pressed_ctrl),
            (false, DragKeys::default()),
        ],
    };
    let effect = start_drag(
        &mut drag_loop,
        &content,
        AllowedEffects::ALL,
        DragButton::Left,
    )
    .unwrap();
    assert_eq!(effect, Effect::Copy);

    let calls = handler.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    let Call::Enter(entered) = &calls[0] else {
        panic!("{calls:?}");
    };
    assert_eq!((entered.x, entered.y), (10, 10));
    assert_eq!(entered.files, work.files);
    assert!(matches!(&calls[1], Call::Over(seen) if seen.keys == pressed));
    assert!(matches!(&calls[2], Call::Over(seen) if seen.keys == pressed_ctrl));
    let Call::Dropped(event, dropped) = &calls[3] else {
        panic!("{calls:?}");
    };
    assert_eq!(event.keys, DragKeys::default());
    assert_eq!(*dropped, content);
}

#[test]
fn escape_cancels_a_whole_drag() {
    let _ole = ole();
    let window = HiddenWindow::new();
    let handler = recorder(|_| Effect::Copy);
    let target = drop_target(window.hwnd(), handler.clone());
    let pressed = DragKeys {
        left_button: true,
        ..DragKeys::default()
    };
    let mut drag_loop = InProcessLoop {
        target: &target,
        window: &window,
        script: vec![(false, pressed), (true, pressed)],
    };
    let content = DragContent {
        text: Some("selected".to_owned()),
        ..DragContent::default()
    };
    let effect = start_drag(
        &mut drag_loop,
        &content,
        AllowedEffects::ALL,
        DragButton::Left,
    )
    .unwrap();
    assert_eq!(effect, Effect::None);
    let calls = handler.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(matches!(calls[0], Call::Enter(_)));
    assert!(matches!(calls[1], Call::Over(_)));
    assert_eq!(calls[2], Call::Leave);
}

#[test]
fn a_drag_without_content_or_a_loop_fails_before_or_in_the_loop() {
    let _ole = ole();
    let mut broken = BrokenLoop { ran: false };
    let nothing = start_drag(
        &mut broken,
        &DragContent::default(),
        AllowedEffects::ALL,
        DragButton::Left,
    );
    assert!(matches!(nothing, Err(WinError::Invalid(_))), "{nothing:?}");
    assert!(!broken.ran);

    let content = DragContent {
        html: Some("<p>x</p>".to_owned()),
        ..DragContent::default()
    };
    let failed = start_drag(
        &mut broken,
        &content,
        AllowedEffects::ALL,
        DragButton::Right,
    );
    assert!(
        matches!(failed, Err(WinError::Invalid("no pointer"))),
        "{failed:?}"
    );
    assert!(broken.ran);
}
