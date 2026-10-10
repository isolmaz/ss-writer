//! The COM shell over [`Store`]: `ITextStoreACP2`, `ITfContextOwnerCompositionSink` and
//! `ITfTextEditSink` on one object (TSF queries the object we hand to `CreateContext` for each),
//! and the plain-text input scope object.
//!
//! windows-rs 0.62 conventions used here:
//!
//! - `#[implement(A, B)] struct T` generates `T_Impl`; the trait implementations are
//!   `impl A_Impl for T_Impl` and reach the fields of `T` through `self`.
//! - Methods with several `[out]` parameters keep raw pointers (TSF may pass null for some);
//!   methods with one `[out]` parameter return `Result<T>`. `RequestLock` returns
//!   `Result<HRESULT>`: `Ok(hr)` becomes `*phrSession` while the call itself is S_OK.
//! - Interface arguments arrive as `Ref<T>`.

#![allow(non_snake_case)]

use std::cell::OnceCell;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;

use ssw_ui::input::ImeAttrStyle;
use windows::Win32::Foundation::{
    E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, HWND, POINT, RECT,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc, FORMATETC, IDataObject,
};
use windows::Win32::System::Ole::CONNECT_E_CANNOTCONNECT;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::TextServices::{
    CLSID_TF_CategoryMgr, CLSID_TF_DisplayAttributeMgr, GUID_PROP_ATTRIBUTE, GUID_PROP_INPUTSCOPE,
    IEnumTfRanges, IS_DEFAULT, ITextStoreACP2, ITextStoreACP2_Impl, ITextStoreACPSink,
    ITfCategoryMgr, ITfCompositionView, ITfContext, ITfContextOwnerCompositionSink,
    ITfContextOwnerCompositionSink_Impl, ITfDisplayAttributeInfo, ITfDisplayAttributeMgr,
    ITfEditRecord, ITfInputScope, ITfInputScope_Impl, ITfProperty, ITfRange, ITfRangeACP,
    ITfTextEditSink, ITfTextEditSink_Impl, InputScope, TF_ANCHOR_END, TF_ATTR_CONVERTED,
    TF_ATTR_FIXEDCONVERTED, TF_ATTR_INPUT_ERROR, TF_ATTR_TARGET_CONVERTED,
    TF_ATTR_TARGET_NOTCONVERTED, TF_DA_ATTR_INFO, TF_DISPLAYATTRIBUTE, TS_AE_END, TS_AE_START,
    TS_ATTRVAL, TS_RT_PLAIN, TS_RUNINFO, TS_SELECTION_ACP, TS_SELECTIONSTYLE, TS_STATUS,
    TS_TEXTCHANGE,
};
use windows::core::{BOOL, BSTR, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, Result, implement};

use super::TextStoreHost;
use super::store::Store;
use super::view::{AcpSelection, TextChange};

/// Writes `value` through an out pointer TSF may pass as null.
fn put<T>(pointer: *mut T, value: T) {
    if !pointer.is_null() {
        // SAFETY: TSF passes either null or a pointer valid for one write of `T`.
        unsafe { pointer.write(value) };
    }
}

/// `count` UTF-16 units at `text`, empty for a null pointer.
fn wide<'a>(text: *const u16, count: u32) -> &'a [u16] {
    if text.is_null() || count == 0 {
        &[]
    } else {
        // SAFETY: TSF passes `count` valid UTF-16 units that stay alive for the call.
        unsafe { std::slice::from_raw_parts(text, count as usize) }
    }
}

fn ts_change(change: TextChange) -> TS_TEXTCHANGE {
    TS_TEXTCHANGE {
        acpStart: change.start as i32,
        acpOldEnd: change.old_end as i32,
        acpNewEnd: change.new_end as i32,
    }
}

/// The canonical `IUnknown` pointer: the COM object identity `AdviseSink`/`UnadviseSink` compare.
fn identity(unknown: &IUnknown) -> Result<usize> {
    Ok(unknown.cast::<IUnknown>()?.as_raw() as usize)
}

/// The ACP extent of a TSF range. Needs no edit cookie.
fn range_extent(range: &ITfRange) -> Option<Range<usize>> {
    let acp: ITfRangeACP = range.cast().ok()?;
    let (mut start, mut count) = (0i32, 0i32);
    // SAFETY: both out pointers are valid for the call.
    unsafe { acp.GetExtent(&mut start, &mut count) }.ok()?;
    let start = start.max(0) as usize;
    Some(start..start + count.max(0) as usize)
}

/// The ACP extent of a composition.
fn composition_extent(view: &ITfCompositionView) -> Option<Range<usize>> {
    // SAFETY: plain call on TSF's live composition view.
    let range = unsafe { view.GetRange() }.ok()?;
    range_extent(&range)
}

/// The IME display style of a `TF_DISPLAYATTRIBUTE::bAttr`.
fn style_of(attribute: TF_DA_ATTR_INFO) -> ImeAttrStyle {
    match attribute {
        TF_ATTR_TARGET_CONVERTED => ImeAttrStyle::TargetConverted,
        TF_ATTR_CONVERTED => ImeAttrStyle::Converted,
        TF_ATTR_TARGET_NOTCONVERTED => ImeAttrStyle::TargetNotConverted,
        TF_ATTR_INPUT_ERROR => ImeAttrStyle::InputError,
        TF_ATTR_FIXEDCONVERTED => ImeAttrStyle::FixedConverted,
        _ => ImeAttrStyle::Input,
    }
}

/// Reads the display attributes (`GUID_PROP_ATTRIBUTE`) an IME attached to its composition.
struct AttributeReader {
    categories: ITfCategoryMgr,
    display: ITfDisplayAttributeMgr,
}

impl AttributeReader {
    fn new() -> Result<Self> {
        // SAFETY: plain in-process activation of the system category and attribute managers.
        unsafe {
            Ok(Self {
                categories: CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?,
                display: CoCreateInstance(
                    &CLSID_TF_DisplayAttributeMgr,
                    None,
                    CLSCTX_INPROC_SERVER,
                )?,
            })
        }
    }

    /// The display style of one attribute run, if it carries a registered display attribute.
    fn style(&self, property: &ITfProperty, cookie: u32, range: &ITfRange) -> Option<ImeAttrStyle> {
        // SAFETY: COM calls on live interfaces with valid out parameters; `cookie` is the
        // read-only edit cookie of the running edit.
        unsafe {
            let value = property.GetValue(cookie, range).ok()?;
            let atom = i32::try_from(&value).ok()?;
            let guid = self.categories.GetGUID(atom as u32).ok()?;
            let mut info: Option<ITfDisplayAttributeInfo> = None;
            let mut owner = GUID::zeroed();
            self.display
                .GetDisplayAttributeInfo(&guid, &mut info, &mut owner)
                .ok()?;
            let mut attribute = TF_DISPLAYATTRIBUTE::default();
            info?.GetAttributeInfo(&mut attribute).ok()?;
            Some(style_of(attribute.bAttr))
        }
    }

    /// The styled runs of the whole context. Must run inside an edit with cookie `cookie`.
    fn read(&self, context: &ITfContext, cookie: u32) -> Result<Vec<(Range<usize>, ImeAttrStyle)>> {
        let mut runs = Vec::new();
        // SAFETY: COM calls on live interfaces with valid out parameters; `cookie` is the
        // read-only edit cookie of the running edit.
        unsafe {
            let property = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            let whole = context.GetStart(cookie)?;
            let end = context.GetEnd(cookie)?;
            whole.ShiftEndToRange(cookie, &end, TF_ANCHOR_END)?;
            let mut ranges: Option<IEnumTfRanges> = None;
            property.EnumRanges(cookie, &mut ranges, &whole)?;
            let Some(ranges) = ranges else {
                return Ok(runs);
            };
            loop {
                let mut next: [Option<ITfRange>; 1] = [None];
                let mut fetched = 0u32;
                ranges.Next(&mut next, &mut fetched)?;
                let Some(range) = next[0].take() else { break };
                if let (Some(style), Some(extent)) =
                    (self.style(&property, cookie, &range), range_extent(&range))
                {
                    runs.push((extent, style));
                }
            }
        }
        Ok(runs)
    }
}

/// The input scope of the editor: plain text. The touch keyboard and dictation read it to pick
/// their layout and behaviour.
#[implement(ITfInputScope)]
struct TextInputScope;

impl ITfInputScope_Impl for TextInputScope_Impl {
    fn GetInputScopes(
        &self,
        pprginputscopes: *mut *mut InputScope,
        pccount: *mut u32,
    ) -> Result<()> {
        if pprginputscopes.is_null() || pccount.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: the caller frees the array with CoTaskMemFree; one InputScope is allocated.
        let scopes = unsafe { CoTaskMemAlloc(size_of::<InputScope>()) }.cast::<InputScope>();
        if scopes.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        // SAFETY: `scopes` is valid for one write of `InputScope`; both out pointers were
        // checked above.
        unsafe {
            scopes.write(IS_DEFAULT);
            *pprginputscopes = scopes;
            *pccount = 1;
        }
        Ok(())
    }

    fn GetPhrase(&self, _ppbstrphrases: *mut *mut BSTR, _pccount: *mut u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetRegularExpression(&self) -> Result<BSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetSRGS(&self) -> Result<BSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetXML(&self) -> Result<BSTR> {
        Err(E_NOTIMPL.into())
    }
}

/// The COM object TSF talks to.
#[implement(ITextStoreACP2, ITfContextOwnerCompositionSink, ITfTextEditSink)]
struct StoreObject {
    store: Rc<Store>,
    attributes: OnceCell<Option<AttributeReader>>,
}

impl ITextStoreACP2_Impl for StoreObject_Impl {
    fn AdviseSink(&self, riid: *const GUID, punk: Ref<IUnknown>, dwmask: u32) -> Result<()> {
        // SAFETY: TSF passes a pointer to the interface id, or null.
        if riid.is_null() || unsafe { *riid } != ITextStoreACPSink::IID {
            return Err(CONNECT_E_CANNOTCONNECT.into());
        }
        let unknown = punk.ok()?;
        // ITextStoreACP2 reuses the ITextStoreACPSink interface.
        let sink: ITextStoreACPSink = unknown.cast()?;
        self.store.advise_sink(sink, identity(unknown)?, dwmask)
    }

    fn UnadviseSink(&self, punk: Ref<IUnknown>) -> Result<()> {
        self.store.unadvise_sink(identity(punk.ok()?)?)
    }

    fn RequestLock(&self, dwlockflags: u32) -> Result<windows::core::HRESULT> {
        self.store.request_lock(dwlockflags)
    }

    fn GetStatus(&self) -> Result<TS_STATUS> {
        Ok(self.store.status())
    }

    fn QueryInsert(
        &self,
        acpteststart: i32,
        acptestend: i32,
        _cch: u32,
        pacpresultstart: *mut i32,
        pacpresultend: *mut i32,
    ) -> Result<()> {
        let (start, end) = self.store.query_insert(acpteststart, acptestend);
        put(pacpresultstart, start as i32);
        put(pacpresultend, end as i32);
        Ok(())
    }

    fn GetSelection(
        &self,
        ulindex: u32,
        ulcount: u32,
        pselection: *mut TS_SELECTION_ACP,
        pcfetched: *mut u32,
    ) -> Result<()> {
        if pselection.is_null() || pcfetched.is_null() {
            return Err(E_POINTER.into());
        }
        put(pcfetched, 0);
        if ulcount == 0 {
            return Ok(());
        }
        let selection = self.store.get_selection(ulindex)?;
        put(
            pselection,
            TS_SELECTION_ACP {
                acpStart: selection.start as i32,
                acpEnd: selection.end as i32,
                style: TS_SELECTIONSTYLE {
                    ase: if selection.backward {
                        TS_AE_START
                    } else {
                        TS_AE_END
                    },
                    fInterimChar: false.into(),
                },
            },
        );
        put(pcfetched, 1);
        Ok(())
    }

    fn SetSelection(&self, ulcount: u32, pselection: *const TS_SELECTION_ACP) -> Result<()> {
        if pselection.is_null() && ulcount > 0 {
            return Err(E_POINTER.into());
        }
        let raw: &[TS_SELECTION_ACP] = if ulcount == 0 {
            &[]
        } else {
            // SAFETY: TSF passes `ulcount` valid selections that stay alive for the call.
            unsafe { std::slice::from_raw_parts(pselection, ulcount as usize) }
        };
        let selections: Vec<AcpSelection> = raw
            .iter()
            .map(|selection| AcpSelection {
                start: selection.acpStart.max(0) as usize,
                end: selection.acpEnd.max(0) as usize,
                backward: selection.style.ase == TS_AE_START,
            })
            .collect();
        self.store.set_selection(&selections)
    }

    fn GetText(
        &self,
        acpstart: i32,
        acpend: i32,
        pchplain: PWSTR,
        cchplainreq: u32,
        pcchplainret: *mut u32,
        prgruninfo: *mut TS_RUNINFO,
        cruninforeq: u32,
        pcruninforet: *mut u32,
        pacpnext: *mut i32,
    ) -> Result<()> {
        // A null buffer or zero capacity is legal ("runs only"): no characters, no runs.
        let want_text = !pchplain.is_null() && cchplainreq > 0;
        let out = self.store.get_text(
            acpstart,
            acpend,
            if want_text { cchplainreq as usize } else { 0 },
            cruninforeq as usize,
        )?;
        if want_text && !out.text.is_empty() {
            // SAFETY: `pchplain` has room for `cchplainreq` units and `get_text` returned at
            // most that many.
            unsafe { std::ptr::copy_nonoverlapping(out.text.as_ptr(), pchplain.0, out.text.len()) };
        }
        put(pcchplainret, out.text.len() as u32);
        if out.run_count > 0 && !prgruninfo.is_null() {
            put(
                prgruninfo,
                TS_RUNINFO {
                    uCount: out.text.len() as u32,
                    r#type: TS_RT_PLAIN,
                },
            );
        }
        put(pcruninforet, out.run_count);
        put(pacpnext, out.next as i32);
        Ok(())
    }

    fn SetText(
        &self,
        _dwflags: u32,
        acpstart: i32,
        acpend: i32,
        pchtext: &PCWSTR,
        cch: u32,
    ) -> Result<TS_TEXTCHANGE> {
        let change = self
            .store
            .set_text(acpstart, acpend, wide(pchtext.0, cch))?;
        Ok(ts_change(change))
    }

    fn GetFormattedText(&self, _acpstart: i32, _acpend: i32) -> Result<IDataObject> {
        Err(E_NOTIMPL.into())
    }

    fn GetEmbedded(
        &self,
        _acppos: i32,
        _rguidservice: *const GUID,
        _riid: *const GUID,
    ) -> Result<IUnknown> {
        Err(E_NOTIMPL.into())
    }

    fn QueryInsertEmbedded(
        &self,
        _pguidservice: *const GUID,
        _pformatetc: *const FORMATETC,
    ) -> Result<BOOL> {
        // A plain-text store: no embedded objects.
        Ok(false.into())
    }

    fn InsertEmbedded(
        &self,
        _dwflags: u32,
        _acpstart: i32,
        _acpend: i32,
        _pdataobject: Ref<IDataObject>,
    ) -> Result<TS_TEXTCHANGE> {
        Err(E_NOTIMPL.into())
    }

    fn InsertTextAtSelection(
        &self,
        dwflags: u32,
        pchtext: &PCWSTR,
        cch: u32,
        pacpstart: *mut i32,
        pacpend: *mut i32,
        pchange: *mut TS_TEXTCHANGE,
    ) -> Result<()> {
        let out = self
            .store
            .insert_text_at_selection(dwflags, wide(pchtext.0, cch))?;
        put(pacpstart, out.start as i32);
        put(pacpend, out.end as i32);
        if let Some(change) = out.change {
            put(pchange, ts_change(change));
        }
        Ok(())
    }

    fn InsertEmbeddedAtSelection(
        &self,
        _dwflags: u32,
        _pdataobject: Ref<IDataObject>,
        _pacpstart: *mut i32,
        _pacpend: *mut i32,
        _pchange: *mut TS_TEXTCHANGE,
    ) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn RequestSupportedAttrs(
        &self,
        _dwflags: u32,
        cfilterattrs: u32,
        pafilterattrs: *const GUID,
    ) -> Result<()> {
        self.store
            .request_attrs(filter(pafilterattrs, cfilterattrs));
        Ok(())
    }

    fn RequestAttrsAtPosition(
        &self,
        _acppos: i32,
        cfilterattrs: u32,
        pafilterattrs: *const GUID,
        _dwflags: u32,
    ) -> Result<()> {
        self.store
            .request_attrs(filter(pafilterattrs, cfilterattrs));
        Ok(())
    }

    fn RequestAttrsTransitioningAtPosition(
        &self,
        _acppos: i32,
        _cfilterattrs: u32,
        _pafilterattrs: *const GUID,
        _dwflags: u32,
    ) -> Result<()> {
        // The input scope never changes inside the text: nothing transitions.
        Ok(())
    }

    fn FindNextAttrTransition(
        &self,
        _acpstart: i32,
        acphalt: i32,
        _cfilterattrs: u32,
        _pafilterattrs: *const GUID,
        _dwflags: u32,
        pacpnext: *mut i32,
        pffound: *mut BOOL,
        plfoundoffset: *mut i32,
    ) -> Result<()> {
        put(pacpnext, acphalt);
        put(pffound, false.into());
        put(plfoundoffset, 0);
        Ok(())
    }

    fn RetrieveRequestedAttrs(
        &self,
        ulcount: u32,
        paattrvals: *mut TS_ATTRVAL,
        pcfetched: *mut u32,
    ) -> Result<()> {
        if pcfetched.is_null() {
            return Err(E_POINTER.into());
        }
        put(pcfetched, 0);
        if ulcount == 0 || paattrvals.is_null() || !self.store.take_input_scope_request() {
            return Ok(());
        }
        let scope: ITfInputScope = TextInputScope.into();
        let unknown: IUnknown = scope.into();
        put(
            paattrvals,
            TS_ATTRVAL {
                idAttr: GUID_PROP_INPUTSCOPE,
                dwOverlapId: 0,
                varValue: VARIANT::from(unknown),
            },
        );
        put(pcfetched, 1);
        Ok(())
    }

    fn GetEndACP(&self) -> Result<i32> {
        Ok(self.store.end_acp()? as i32)
    }

    fn GetActiveView(&self) -> Result<u32> {
        // One view, cookie 0 (TS_VCOOKIE_NUL is 0xFFFFFFFF, not 0).
        Ok(0)
    }

    fn GetACPFromPoint(&self, vcview: u32, ptscreen: *const POINT, _dwflags: u32) -> Result<i32> {
        if ptscreen.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: checked non-null; TSF passes a valid POINT.
        let point = unsafe { *ptscreen };
        Ok(self.store.acp_from_point(vcview, point)? as i32)
    }

    fn GetTextExt(
        &self,
        vcview: u32,
        acpstart: i32,
        acpend: i32,
        prc: *mut RECT,
        pfclipped: *mut BOOL,
    ) -> Result<()> {
        if prc.is_null() || pfclipped.is_null() {
            return Err(E_POINTER.into());
        }
        let (rect, clipped) = self.store.text_ext(vcview, acpstart, acpend)?;
        put(prc, rect);
        put(pfclipped, clipped.into());
        Ok(())
    }

    fn GetScreenExt(&self, vcview: u32) -> Result<RECT> {
        self.store.screen_ext(vcview)
    }
}

/// The attribute ids of a TSF filter list.
fn filter<'a>(attributes: *const GUID, count: u32) -> &'a [GUID] {
    if attributes.is_null() || count == 0 {
        &[]
    } else {
        // SAFETY: TSF passes `count` valid GUIDs that stay alive for the call.
        unsafe { std::slice::from_raw_parts(attributes, count as usize) }
    }
}

impl ITfContextOwnerCompositionSink_Impl for StoreObject_Impl {
    fn OnStartComposition(&self, pcomposition: Ref<ITfCompositionView>) -> Result<BOOL> {
        let extent = pcomposition.as_ref().and_then(composition_extent);
        // A rejected composition makes the IME's `StartComposition` fail.
        Ok(self.store.composition_started(extent).into())
    }

    fn OnUpdateComposition(
        &self,
        _pcomposition: Ref<ITfCompositionView>,
        prangenew: Ref<ITfRange>,
    ) -> Result<()> {
        // `prangenew` is null when only the content, not the extent, changed.
        self.store
            .composition_updated(prangenew.as_ref().and_then(range_extent));
        Ok(())
    }

    fn OnEndComposition(&self, _pcomposition: Ref<ITfCompositionView>) -> Result<()> {
        self.store.composition_ended();
        Ok(())
    }
}

impl ITfTextEditSink_Impl for StoreObject_Impl {
    fn OnEndEdit(
        &self,
        pic: Ref<ITfContext>,
        ecreadonly: u32,
        _peditrecord: Ref<ITfEditRecord>,
    ) -> Result<()> {
        // The display attributes are only needed while composing; the read-only cookie of the
        // edit that just ended lets us read them without requesting an edit session.
        if !self.store.is_composing() {
            return Ok(());
        }
        let (Some(context), Some(reader)) = (
            pic.as_ref(),
            self.attributes
                .get_or_init(|| AttributeReader::new().ok())
                .as_ref(),
        ) else {
            return Ok(());
        };
        if let Ok(runs) = reader.read(context, ecreadonly) {
            self.store.display_attributes(&runs);
        }
        Ok(())
    }
}

/// An `ITextStoreACP2` over a [`TextStoreHost`].
///
/// Hand [`TextStore::interface`] to `ITfDocumentMgr::CreateContext` ([`TextServices`](super::TextServices)
/// does) and tell the store about host changes with the notification methods below. The
/// notifications are ignored while the store executes an edit TSF requested (see the
/// [module documentation](super)), so the host may call them unconditionally from its own
/// edit path.
pub struct TextStore {
    store: Rc<Store>,
    interface: ITextStoreACP2,
}

impl fmt::Debug for TextStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextStore")
            .field("composing", &self.is_composing())
            .finish_non_exhaustive()
    }
}

impl TextStore {
    /// A store over `host`, answering layout queries for the window `hwnd`.
    #[must_use]
    pub fn new(hwnd: HWND, host: Rc<dyn TextStoreHost>) -> Self {
        let store = Rc::new(Store::new(hwnd, host));
        let interface: ITextStoreACP2 = StoreObject {
            store: store.clone(),
            attributes: OnceCell::new(),
        }
        .into();
        Self { store, interface }
    }

    /// The COM interface TSF drives.
    #[must_use]
    pub fn interface(&self) -> &ITextStoreACP2 {
        &self.interface
    }

    /// The host replaced the bytes `start..start + removed.len()` of the document, whose old
    /// text was `removed`, by `inserted`. Call it after the document changed, outside the
    /// host's own borrows; it also reports the selection as changed.
    pub fn text_changed(&self, start: usize, removed: &str, inserted: &str) {
        self.store.text_changed(start, removed, inserted);
    }

    /// The host moved the selection (without editing text).
    pub fn selection_changed(&self) {
        self.store.selection_changed();
    }

    /// Text rectangles changed: scrolling, resizing, zoom, fonts, DPI, window moves.
    pub fn layout_changed(&self) {
        self.store.layout_changed();
    }

    /// The editor can answer layout queries now (call once it has a layout).
    pub fn layout_created(&self) {
        self.store.layout_created();
    }

    /// The editor view is going away (call before the store is torn down).
    pub fn layout_destroyed(&self) {
        self.store.layout_destroyed();
    }

    /// [`TextStoreHost::is_read_only`] changed.
    pub fn status_changed(&self) {
        self.store.status_changed();
    }

    /// True while an IME composition is active.
    #[must_use]
    pub fn is_composing(&self) -> bool {
        self.store.is_composing()
    }
}
