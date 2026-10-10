//! OLE drag and drop (spec §2.4, §5): a drop target for files (`CF_HDROP`), text
//! (`CF_UNICODETEXT`) and HTML (`HTML Format`), and the drag source side (`IDropSource` plus an
//! `IDataObject`) for dragging text, HTML and files out of the app.
//!
//! # Dropping onto the app
//!
//! [`WindowDropTarget::register`] makes a window a drop target. The OLE calls are translated
//! into [`DropHandler`] callbacks that carry platform-neutral data: client pixels, the held
//! keys ([`DragKeys`]), the effects the source allows ([`AllowedEffects`]), the kinds of data
//! offered ([`DropKinds`]) and the dragged file paths. The handler answers with an [`Effect`];
//! an effect the source does not allow is turned into [`Effect::None`].
//!
//! The effect for dropping files follows the Windows rules ([`choose_effect`]):
//!
//! | keys                | effect                                         |
//! |---------------------|------------------------------------------------|
//! | Ctrl + Shift or Alt | link                                           |
//! | Shift               | move                                           |
//! | Ctrl                | copy                                           |
//! | none                | move on the same volume as the target, else copy |
//!
//! The decision is data: [`DragEvent::effect_into_folder`] applies the rules for a folder of
//! the sidebar tree, [`DragEvent::effect_into_document`] for an editor (where the app inserts a
//! link or the text, and may answer [`Effect::Link`] or [`Effect::Copy`] as it sees fit).
//!
//! # Dragging out of the app
//!
//! [`data_object`] wraps a [`DragContent`] in an `IDataObject`, [`drop_source`] is the matching
//! `IDropSource` ([`drag_progress`] is its button / Escape table) and [`start_drag`] runs the
//! modal drag through a [`DragLoop`]: [`OleDragLoop`] calls `DoDragDrop`, tests substitute a loop
//! that drives a real [`IDropTarget`] in-process.
//!
//! OLE drag and drop needs `OleInitialize` on the thread ([`OleGuard`]).

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::LazyLock;

use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_DVASPECT, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_NOTIMPL, E_POINTER, E_UNEXPECTED,
    GlobalFree, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, POINTL, S_OK,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Storage::FileSystem::GetVolumePathNameW;
use windows::Win32::System::Com::{
    APTTYPE, APTTYPE_MAINSTA, APTTYPE_STA, APTTYPEQUALIFIER, CLSCTX_INPROC_SERVER,
    CoCreateInstance, CoGetApartmentType, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink,
    IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0,
    TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
    OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
};
use windows::Win32::System::SystemServices::{
    MK_CONTROL, MK_LBUTTON, MK_RBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS,
};
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDropTargetHelper, SHCreateStdEnumFmtEtc};
use windows::core::{BOOL, Error, HRESULT, HSTRING, Ref, implement};

use crate::com::{OsResultExt, WinError, from_wide};
use crate::services::clipboard::{
    FORMAT_HTML, build_cf_html, build_hdrop, decode_text, encode_text, parse_cf_html, parse_hdrop,
    register,
};

/// `MK_ALT`: Alt is held (the OLE key-state flag has no windows-rs constant).
const MK_ALT: u32 = 0x20;

/// What a drop does with the data, as the target answers and as the source is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// The drop is refused (the "no" cursor).
    None,
    /// The data is copied.
    Copy,
    /// The data is moved: a source told `Move` removes what it offered.
    Move,
    /// A link to the data is made.
    Link,
}

impl Effect {
    /// The `DROPEFFECT_*` value of this effect.
    #[must_use]
    pub fn to_ole(self) -> DROPEFFECT {
        match self {
            Effect::None => DROPEFFECT_NONE,
            Effect::Copy => DROPEFFECT_COPY,
            Effect::Move => DROPEFFECT_MOVE,
            Effect::Link => DROPEFFECT_LINK,
        }
    }

    /// The effect `ole` names. A set of effects resolves to move, then copy, then link.
    #[must_use]
    pub fn from_ole(ole: DROPEFFECT) -> Self {
        if ole.contains(DROPEFFECT_MOVE) {
            Effect::Move
        } else if ole.contains(DROPEFFECT_COPY) {
            Effect::Copy
        } else if ole.contains(DROPEFFECT_LINK) {
            Effect::Link
        } else {
            Effect::None
        }
    }
}

/// The effects a drag source allows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AllowedEffects {
    /// Copying is allowed.
    pub copy: bool,
    /// Moving is allowed.
    pub moves: bool,
    /// Linking is allowed.
    pub link: bool,
}

impl AllowedEffects {
    /// Every effect.
    pub const ALL: Self = Self {
        copy: true,
        moves: true,
        link: true,
    };

    /// The effects named by the `DROPEFFECT_*` flags `ole`.
    #[must_use]
    pub fn from_ole(ole: DROPEFFECT) -> Self {
        Self {
            copy: ole.contains(DROPEFFECT_COPY),
            moves: ole.contains(DROPEFFECT_MOVE),
            link: ole.contains(DROPEFFECT_LINK),
        }
    }

    /// The `DROPEFFECT_*` flags of the allowed effects.
    #[must_use]
    pub fn to_ole(self) -> DROPEFFECT {
        let mut ole = DROPEFFECT_NONE;
        if self.copy {
            ole |= DROPEFFECT_COPY;
        }
        if self.moves {
            ole |= DROPEFFECT_MOVE;
        }
        if self.link {
            ole |= DROPEFFECT_LINK;
        }
        ole
    }

    /// Whether `effect` is allowed. Refusing ([`Effect::None`]) always is.
    #[must_use]
    pub fn allows(self, effect: Effect) -> bool {
        match effect {
            Effect::None => true,
            Effect::Copy => self.copy,
            Effect::Move => self.moves,
            Effect::Link => self.link,
        }
    }
}

/// The modifier keys and mouse buttons held during a drag.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DragKeys {
    /// Shift.
    pub shift: bool,
    /// Ctrl.
    pub ctrl: bool,
    /// Alt.
    pub alt: bool,
    /// The left mouse button.
    pub left_button: bool,
    /// The right mouse button.
    pub right_button: bool,
}

impl DragKeys {
    /// The keys named by the `MK_*` flags of an OLE key state.
    #[must_use]
    pub fn from_ole(flags: MODIFIERKEYS_FLAGS) -> Self {
        Self {
            shift: flags.contains(MK_SHIFT),
            ctrl: flags.contains(MK_CONTROL),
            alt: flags.contains(MODIFIERKEYS_FLAGS(MK_ALT)),
            left_button: flags.contains(MK_LBUTTON),
            right_button: flags.contains(MK_RBUTTON),
        }
    }

    /// The `MK_*` flags of these keys.
    #[must_use]
    pub fn to_ole(self) -> MODIFIERKEYS_FLAGS {
        let mut flags = MODIFIERKEYS_FLAGS(0);
        if self.shift {
            flags |= MK_SHIFT;
        }
        if self.ctrl {
            flags |= MK_CONTROL;
        }
        if self.alt {
            flags |= MODIFIERKEYS_FLAGS(MK_ALT);
        }
        if self.left_button {
            flags |= MK_LBUTTON;
        }
        if self.right_button {
            flags |= MK_RBUTTON;
        }
        flags
    }
}

/// The effect Windows picks for a drop (spec §5): Ctrl+Shift or Alt link, Shift moves, Ctrl
/// copies; without keys the data moves when it stays on the target's volume (`same_volume`) and
/// is copied otherwise. A key that asks for an effect the source does not allow gives
/// [`Effect::None`]; without keys the first allowed of the default, the other of copy / move,
/// and link is used.
#[must_use]
pub fn choose_effect(keys: DragKeys, allowed: AllowedEffects, same_volume: bool) -> Effect {
    let asked = if keys.alt || (keys.shift && keys.ctrl) {
        Some(Effect::Link)
    } else if keys.shift {
        Some(Effect::Move)
    } else if keys.ctrl {
        Some(Effect::Copy)
    } else {
        None
    };
    if let Some(effect) = asked {
        return if allowed.allows(effect) {
            effect
        } else {
            Effect::None
        };
    }
    let order = if same_volume {
        [Effect::Move, Effect::Copy, Effect::Link]
    } else {
        [Effect::Copy, Effect::Move, Effect::Link]
    };
    order
        .into_iter()
        .find(|&effect| allowed.allows(effect))
        .unwrap_or(Effect::None)
}

/// The lower-cased mount point (`c:\`) or share root of `path`; `None` when the system cannot
/// tell.
fn volume_of(path: &Path) -> Option<String> {
    let mut buffer = [0u16; 1024];
    // SAFETY: `buffer` is writable for its whole length; the call NUL-terminates it on success.
    unsafe { GetVolumePathNameW(&HSTRING::from(path.as_os_str()), &mut buffer) }.ok()?;
    Some(from_wide(&buffer).to_lowercase())
}

/// Whether every file of `files` lies on the same volume as `folder`. False for no files and
/// when a volume cannot be determined (the drop then copies).
#[must_use]
pub fn files_share_volume(files: &[PathBuf], folder: &Path) -> bool {
    let Some(target) = volume_of(folder) else {
        return false;
    };
    !files.is_empty()
        && files
            .iter()
            .all(|file| volume_of(file).is_some_and(|volume| volume == target))
}

/// The kinds of data a data object offers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DropKinds {
    /// A list of files (`CF_HDROP`).
    pub files: bool,
    /// Unicode text (`CF_UNICODETEXT`).
    pub text: bool,
    /// HTML (`HTML Format`).
    pub html: bool,
}

impl DropKinds {
    /// The kinds `data` offers (asked with `QueryGetData`, nothing is read).
    #[must_use]
    pub fn of(data: &IDataObject) -> Self {
        Self {
            files: supports(data, CF_HDROP.0),
            text: supports(data, CF_UNICODETEXT.0),
            html: supports(data, html_format()),
        }
    }

    /// Whether anything the app accepts is offered.
    #[must_use]
    pub fn any(self) -> bool {
        self.files || self.text || self.html
    }
}

/// The data of a drag: what a drop receives and what a drag out offers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DragContent {
    /// Dragged files and folders.
    pub files: Vec<PathBuf>,
    /// Plain text (line breaks as `\n`).
    pub text: Option<String>,
    /// An HTML fragment.
    pub html: Option<String>,
}

impl DragContent {
    /// Reads every supported format of `data`. Formats the source does not offer (or offers in
    /// a medium other than global memory) stay empty.
    #[must_use]
    pub fn from_data_object(data: &IDataObject) -> Self {
        Self {
            files: read_files(data),
            text: fetch(data, CF_UNICODETEXT.0).map(|bytes| decode_text(&bytes)),
            html: fetch(data, html_format()).and_then(|bytes| parse_cf_html(&bytes)),
        }
    }

    /// Whether there is nothing to drag.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.text.is_none() && self.html.is_none()
    }
}

/// A moment of a drag over the drop target, platform-neutral.
#[derive(Debug, Clone, Copy)]
pub struct DragEvent<'a> {
    /// Pointer x in client pixels of the target window.
    pub x: i32,
    /// Pointer y in client pixels of the target window.
    pub y: i32,
    /// The keys and buttons held.
    pub keys: DragKeys,
    /// The effects the source allows.
    pub allowed: AllowedEffects,
    /// What the dragged data offers.
    pub kinds: DropKinds,
    /// The dragged files (read when the drag entered; empty without files).
    pub files: &'a [PathBuf],
}

impl DragEvent<'_> {
    /// The effect for dropping the dragged files into `folder`: Windows' rules with the volume
    /// of the files compared to the volume of `folder` ([`choose_effect`]).
    #[must_use]
    pub fn effect_into_folder(&self, folder: &Path) -> Effect {
        choose_effect(
            self.keys,
            self.allowed,
            files_share_volume(self.files, folder),
        )
    }

    /// The effect for dropping the dragged data into a document: copy unless a key asks for
    /// another effect ([`choose_effect`] without a volume).
    #[must_use]
    pub fn effect_into_document(&self) -> Effect {
        choose_effect(self.keys, self.allowed, false)
    }
}

/// The app side of a drop target. Methods take `&self` (OLE may call again while one runs, for
/// example from a nested message loop): use interior mutability.
///
/// Only data of a kind the app accepts ([`DropKinds::any`]) reaches the handler; everything
/// else is refused without a call.
pub trait DropHandler {
    /// A drag entered the window. The returned effect is the first answer shown to the user.
    fn drag_enter(&self, event: &DragEvent<'_>) -> Effect;

    /// The pointer moved or a key changed. The returned effect picks the cursor.
    fn drag_over(&self, event: &DragEvent<'_>) -> Effect;

    /// The drag left the window (or was cancelled) without dropping.
    fn drag_leave(&self);

    /// The data was dropped. The returned effect is reported to the drag source: report
    /// [`Effect::Move`] only when the source must remove what it offered. A handler that moved
    /// files itself reports [`Effect::Copy`]; Explorer would delete originals that are
    /// already gone.
    fn dropped(&self, event: &DragEvent<'_>, content: &DragContent) -> Effect;
}

/// The id of the registered `HTML Format` clipboard format.
fn html_format() -> u16 {
    // Registered clipboard format ids lie in 0xC000..=0xFFFF.
    static FORMAT: LazyLock<u16> = LazyLock::new(|| register(FORMAT_HTML) as u16);
    *FORMAT
}

/// A request for `format` as global memory of the content aspect.
fn format_etc(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Whether `data` says it can deliver `format` as global memory.
fn supports(data: &IDataObject, format: u16) -> bool {
    let wanted = format_etc(format);
    // SAFETY: `wanted` is a valid FORMATETC for the duration of the call.
    unsafe { data.QueryGetData(&wanted) }.is_ok()
}

/// The bytes of `format` in `data`, when it delivers them as global memory.
fn fetch(data: &IDataObject, format: u16) -> Option<Vec<u8>> {
    let wanted = format_etc(format);
    // SAFETY: `wanted` is a valid FORMATETC for the duration of the call.
    let mut medium = unsafe { data.GetData(&wanted) }.ok()?;
    let bytes = if medium.tymed == TYMED_HGLOBAL.0 as u32 {
        // SAFETY: `tymed` says the union holds an `HGLOBAL`.
        read_global(unsafe { medium.u.hGlobal })
    } else {
        None
    };
    // SAFETY: `medium` was filled by the source's `GetData`; it is released exactly once, here.
    unsafe { ReleaseStgMedium(&mut medium) };
    bytes
}

/// The paths of the `CF_HDROP` of `data` (empty without one).
fn read_files(data: &IDataObject) -> Vec<PathBuf> {
    fetch(data, CF_HDROP.0)
        .map(|bytes| parse_hdrop(&bytes))
        .unwrap_or_default()
}

/// A copy of the bytes of the global memory block `global`.
fn read_global(global: HGLOBAL) -> Option<Vec<u8>> {
    // SAFETY: `global` is a block owned by a storage medium the caller still holds; it is
    // locked only while the bytes are copied.
    unsafe {
        let size = GlobalSize(global);
        let pointer = GlobalLock(global).cast::<u8>();
        if pointer.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

/// A new movable global memory block holding `bytes`; the receiver of the storage medium owns
/// it.
fn alloc_global(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
    // SAFETY: the block is fresh and large enough; it is locked only for the copy and freed
    // when it cannot be locked.
    unsafe {
        let global = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let pointer = GlobalLock(global).cast::<u8>();
        if pointer.is_null() {
            let error = Error::from_thread();
            let _ = GlobalFree(Some(global));
            return Err(error);
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
        let _ = GlobalUnlock(global);
        Ok(global)
    }
}

/// The effects the source allows, from OLE's in / out effect parameter.
fn allowed_effects(effect: *mut DROPEFFECT) -> windows::core::Result<AllowedEffects> {
    // SAFETY: OLE passes a valid in / out pointer; a null one is rejected.
    unsafe { effect.as_ref() }
        .map(|ole| AllowedEffects::from_ole(*ole))
        .ok_or_else(|| Error::from(E_POINTER))
}

/// Writes the answer into OLE's in / out effect parameter validated by [`allowed_effects`].
fn set_effect(out: *mut DROPEFFECT, effect: Effect) {
    // SAFETY: the pointer was checked for null in the same call by `allowed_effects`.
    unsafe { out.write(effect.to_ole()) };
}

/// What `DragEnter` learned about the data object, kept until the drag ends.
struct Session {
    kinds: DropKinds,
    files: Vec<PathBuf>,
}

/// The `IDropTarget` of one window.
#[implement(IDropTarget)]
struct DropTarget {
    window: HWND,
    handler: Rc<dyn DropHandler>,
    /// Shared so that a callback that re-enters the target sees consistent state.
    session: RefCell<Option<Rc<Session>>>,
    /// The shell's drop target helper: it draws the image of what Explorer drags over the
    /// window (the image disappears over a target that does not forward to it). `None` when
    /// the shell does not provide one; the drag then works without the image.
    helper: Option<IDropTargetHelper>,
}

impl DropTarget {
    /// Forwards a drag event to the shell's drop target helper. Its answers do not matter.
    fn forward(&self, call: impl FnOnce(&IDropTargetHelper) -> windows::core::Result<()>) {
        if let Some(helper) = &self.helper {
            let _ = call(helper);
        }
    }

    /// The screen point `pt` in client pixels of the window (unchanged when the window is
    /// gone).
    fn client_point(&self, pt: &POINTL) -> (i32, i32) {
        let mut point = POINT { x: pt.x, y: pt.y };
        // SAFETY: `point` is a valid in / out point; a failure leaves it in screen coordinates.
        let _ = unsafe { ScreenToClient(self.window, &mut point) };
        (point.x, point.y)
    }

    /// Asks the handler about the current drag; `ask` picks which callback. Refuses without a
    /// session and for data of no accepted kind.
    fn consult(
        &self,
        keys: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        allowed: AllowedEffects,
        ask: impl FnOnce(&dyn DropHandler, &DragEvent<'_>) -> Effect,
    ) -> windows::core::Result<Effect> {
        let session = self
            .session
            .borrow()
            .clone()
            .ok_or_else(|| Error::from(E_UNEXPECTED))?;
        if !session.kinds.any() {
            return Ok(Effect::None);
        }
        let (x, y) = self.client_point(pt);
        let event = DragEvent {
            x,
            y,
            keys: DragKeys::from_ole(keys),
            allowed,
            kinds: session.kinds,
            files: &session.files,
        };
        let effect = ask(self.handler.as_ref(), &event);
        Ok(if allowed.allows(effect) {
            effect
        } else {
            Effect::None
        })
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let data = pdataobj.ok()?;
        let allowed = allowed_effects(pdweffect)?;
        let kinds = DropKinds::of(data);
        let files = if kinds.files {
            read_files(data)
        } else {
            Vec::new()
        };
        *self.session.borrow_mut() = Some(Rc::new(Session { kinds, files }));
        let effect = self.consult(grfkeystate, pt, allowed, |handler, event| {
            handler.drag_enter(event)
        })?;
        set_effect(pdweffect, effect);
        let screen = POINT { x: pt.x, y: pt.y };
        self.forward(|helper| {
            // SAFETY: `data` and `screen` outlive the call; the window is ours.
            unsafe { helper.DragEnter(self.window, data, &screen, effect.to_ole()) }
        });
        Ok(())
    }

    fn DragOver(
        &self,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let allowed = allowed_effects(pdweffect)?;
        let effect = self.consult(grfkeystate, pt, allowed, |handler, event| {
            handler.drag_over(event)
        })?;
        set_effect(pdweffect, effect);
        let screen = POINT { x: pt.x, y: pt.y };
        self.forward(|helper| {
            // SAFETY: `screen` outlives the call.
            unsafe { helper.DragOver(&screen, effect.to_ole()) }
        });
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        let session = self.session.borrow_mut().take();
        if let Some(session) = session {
            // SAFETY: a plain call on the helper, which saw the matching `DragEnter`.
            self.forward(|helper| unsafe { helper.DragLeave() });
            if session.kinds.any() {
                self.handler.drag_leave();
            }
        }
        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let data = pdataobj.ok()?;
        let allowed = allowed_effects(pdweffect)?;
        let content = DragContent::from_data_object(data);
        let outcome = self.consult(grfkeystate, pt, allowed, |handler, event| {
            handler.dropped(event, &content)
        });
        *self.session.borrow_mut() = None;
        let effect = outcome?;
        set_effect(pdweffect, effect);
        let screen = POINT { x: pt.x, y: pt.y };
        self.forward(|helper| {
            // SAFETY: `data` and `screen` outlive the call.
            unsafe { helper.Drop(data, &screen, effect.to_ole()) }
        });
        Ok(())
    }
}

/// An `IDropTarget` that reports to `handler`, with positions relative to `window`'s client
/// area. Register it with `RegisterDragDrop` (see [`WindowDropTarget`]). Create it on a thread
/// with OLE initialized ([`OleGuard`]): the shell's drag image helper is created with it, and
/// only on a single-threaded apartment (a drop target is only valid there; on another apartment
/// the helper would be reached through a proxy that marshals the data object).
#[must_use]
pub fn drop_target(window: HWND, handler: Rc<dyn DropHandler>) -> IDropTarget {
    DropTarget {
        window,
        handler,
        session: RefCell::new(None),
        helper: shell_helper(),
    }
    .into()
}

/// The shell's drop target helper of the calling thread; `None` outside a single-threaded
/// apartment or when the shell does not provide one (the drag then works without the image).
fn shell_helper() -> Option<IDropTargetHelper> {
    let mut apartment = APTTYPE::default();
    let mut qualifier = APTTYPEQUALIFIER::default();
    // SAFETY: both out values are valid for the call; COM not being initialized is an error.
    unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) }.ok()?;
    if apartment != APTTYPE_STA && apartment != APTTYPE_MAINSTA {
        return None;
    }
    // SAFETY: plain in-process creation of the shell's helper on this thread's apartment.
    unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.ok()
}

/// Keeps OLE initialized on the creating thread; uninitializes on drop.
///
/// `RegisterDragDrop` and `DoDragDrop` need `OleInitialize` (which initializes a single-threaded
/// apartment), not only `CoInitialize`. Like [`crate::com::ComGuard`], a guard must be dropped
/// after every COM object of its thread.
#[derive(Debug)]
pub struct OleGuard(());

impl OleGuard {
    /// Initializes OLE (and the single-threaded apartment) on the calling thread.
    ///
    /// # Errors
    /// Fails when the thread already runs in a multi-threaded apartment.
    pub fn initialize() -> Result<Self, WinError> {
        // SAFETY: plain OLE initialization of the calling thread; `Drop` balances it (also for
        // `S_FALSE`, which counts as a successful nested initialization).
        unsafe { OleInitialize(None) }.ctx("OleInitialize")?;
        Ok(Self(()))
    }
}

impl Drop for OleGuard {
    fn drop(&mut self) {
        // SAFETY: balances the successful `OleInitialize` of `initialize` on this thread.
        unsafe { OleUninitialize() };
    }
}

/// A window registered as a drop target; revoked on drop.
#[derive(Debug)]
pub struct WindowDropTarget {
    window: HWND,
}

impl WindowDropTarget {
    /// Registers `window` (of this thread, OLE initialized: [`OleGuard`]) as a drop target that
    /// reports to `handler`.
    ///
    /// # Errors
    /// The window is invalid, already a drop target, or OLE is not initialized.
    pub fn register(window: HWND, handler: Rc<dyn DropHandler>) -> Result<Self, WinError> {
        let target = drop_target(window, handler);
        // SAFETY: `window` is a window handle of this thread and `target` a live drop target;
        // OLE keeps its own reference after the call.
        unsafe { RegisterDragDrop(window, &target) }.ctx("RegisterDragDrop")?;
        Ok(Self { window })
    }
}

impl Drop for WindowDropTarget {
    fn drop(&mut self) {
        // SAFETY: undoes the registration of `register`; a window that is already gone has
        // nothing left to revoke.
        let _ = unsafe { RevokeDragDrop(self.window) };
    }
}

/// One format of a data object with its bytes.
struct Rendered {
    format: u16,
    bytes: Vec<u8>,
}

/// The `IDataObject` of a drag out of the app: every format is global memory.
#[implement(IDataObject)]
struct OutgoingData {
    items: Vec<Rendered>,
}

impl OutgoingData {
    /// The item `wanted` asks for, or the `DV_E_*` error that says why there is none.
    fn lookup(&self, wanted: *const FORMATETC) -> windows::core::Result<&Rendered> {
        // SAFETY: COM callers pass a valid FORMATETC; null is rejected.
        let wanted = unsafe { wanted.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        let item = self
            .items
            .iter()
            .find(|item| item.format == wanted.cfFormat)
            .ok_or_else(|| Error::from(DV_E_FORMATETC))?;
        if wanted.dwAspect != DVASPECT_CONTENT.0 {
            return Err(DV_E_DVASPECT.into());
        }
        if wanted.lindex != -1 {
            return Err(DV_E_LINDEX.into());
        }
        if wanted.tymed & TYMED_HGLOBAL.0 as u32 == 0 {
            return Err(DV_E_TYMED.into());
        }
        Ok(item)
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for OutgoingData_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let item = self.lookup(pformatetcin)?;
        let global = alloc_global(&item.bytes)?;
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: global },
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
        match self.lookup(pformatetc) {
            Ok(_) => S_OK,
            Err(error) => error.code(),
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        // SAFETY: COM callers pass a valid input FORMATETC and writable output; null is
        // rejected.
        match unsafe { pformatectin.as_ref() } {
            Some(wanted) if !pformatetcout.is_null() => {
                // SAFETY: `pformatetcout` is non-null and writable (see above).
                unsafe {
                    pformatetcout.write(FORMATETC {
                        ptd: ptr::null_mut(),
                        ..*wanted
                    });
                }
                DATA_S_SAMEFORMATETC
            }
            _ => E_POINTER,
        }
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats: Vec<FORMATETC> = self
            .items
            .iter()
            .map(|item| format_etc(item.format))
            .collect();
        // SAFETY: `formats` is a valid array for the call; the shell enumerator copies it.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<'_, IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// A data object for dragging `content` out of the app: `CF_HDROP` for files,
/// `CF_UNICODETEXT` for text and `HTML Format` for HTML (in this order), each as global
/// memory.
///
/// # Errors
/// `content` is empty.
pub fn data_object(content: &DragContent) -> Result<IDataObject, WinError> {
    let mut items = Vec::new();
    if !content.files.is_empty() {
        items.push(Rendered {
            format: CF_HDROP.0,
            bytes: build_hdrop(&content.files),
        });
    }
    if let Some(text) = &content.text {
        items.push(Rendered {
            format: CF_UNICODETEXT.0,
            bytes: encode_text(text),
        });
    }
    if let Some(html) = &content.html {
        items.push(Rendered {
            format: html_format(),
            bytes: build_cf_html(html),
        });
    }
    if items.is_empty() {
        return Err(WinError::Invalid("a drag needs files, text or HTML"));
    }
    Ok(OutgoingData { items }.into())
}

/// The mouse button that started a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragButton {
    /// The left button.
    Left,
    /// The right button.
    Right,
}

/// What the drag source tells OLE after a key or button change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragProgress {
    /// Keep dragging.
    Continue,
    /// Drop here.
    Drop,
    /// Abandon the drag.
    Cancel,
}

/// The answer of a drag source (`IDropSource::QueryContinueDrag`): Escape or the other mouse
/// button cancels, releasing the button that started the drag drops, anything else continues.
#[must_use]
pub fn drag_progress(escape: bool, keys: DragKeys, button: DragButton) -> DragProgress {
    let (held, other) = match button {
        DragButton::Left => (keys.left_button, keys.right_button),
        DragButton::Right => (keys.right_button, keys.left_button),
    };
    if escape || other {
        DragProgress::Cancel
    } else if held {
        DragProgress::Continue
    } else {
        DragProgress::Drop
    }
}

/// The `IDropSource` of a drag started with `button`.
#[implement(IDropSource)]
struct DragSource {
    button: DragButton,
}

#[allow(non_snake_case)]
impl IDropSource_Impl for DragSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        match drag_progress(
            fescapepressed.as_bool(),
            DragKeys::from_ole(grfkeystate),
            self.button,
        ) {
            DragProgress::Continue => S_OK,
            DragProgress::Drop => DRAGDROP_S_DROP,
            DragProgress::Cancel => DRAGDROP_S_CANCEL,
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// The `IDropSource` for a drag started with `button`: [`drag_progress`] decides when it ends,
/// the system cursors show the effects.
#[must_use]
pub fn drop_source(button: DragButton) -> IDropSource {
    DragSource { button }.into()
}

/// The modal part of a drag out of the app.
pub trait DragLoop {
    /// Runs the drag of `data` with `source` until it is dropped or cancelled; the effect the
    /// target reported ([`Effect::None`] when cancelled).
    ///
    /// # Errors
    /// The drag could not run.
    fn run(
        &mut self,
        data: &IDataObject,
        source: &IDropSource,
        allowed: AllowedEffects,
    ) -> Result<Effect, WinError>;
}

/// The real [`DragLoop`]: `DoDragDrop`. It blocks, driven by the pointer, until the drag ends;
/// the thread needs [`OleGuard`].
#[derive(Debug, Clone, Copy, Default)]
pub struct OleDragLoop;

impl DragLoop for OleDragLoop {
    fn run(
        &mut self,
        data: &IDataObject,
        source: &IDropSource,
        allowed: AllowedEffects,
    ) -> Result<Effect, WinError> {
        let mut effect = DROPEFFECT_NONE;
        // SAFETY: both objects are live and `effect` outlives the call, which runs its own
        // modal loop on this thread.
        let result = unsafe { DoDragDrop(data, source, allowed.to_ole(), &mut effect) };
        if result == DRAGDROP_S_DROP {
            Ok(Effect::from_ole(effect))
        } else if result == DRAGDROP_S_CANCEL {
            Ok(Effect::None)
        } else {
            Err(WinError::from_os("DoDragDrop", result.into()))
        }
    }
}

/// Drags `content` out of the app through `drag_loop`, started with `button`. The effect the
/// target reported: [`Effect::Move`] means the caller removes what it offered.
///
/// # Errors
/// `content` is empty or the drag could not run.
pub fn start_drag<L: DragLoop>(
    drag_loop: &mut L,
    content: &DragContent,
    allowed: AllowedEffects,
    button: DragButton,
) -> Result<Effect, WinError> {
    let data = data_object(content)?;
    let source = drop_source(button);
    drag_loop.run(&data, &source, allowed)
}
