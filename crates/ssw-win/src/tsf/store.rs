//! The text store core: `ITextStoreACP2` semantics, the lock state machine, change notifications
//! and the IME composition, without any COM object of our own (the COM shell is in `com.rs`).
//!
//! # Lock state machine
//!
//! ```text
//!             RequestLock(sync|async, R/RW)           OnLockGranted(R/RW) returns
//!   Unlocked ------------------------------> Locked(R|RW) ----------------------> Unlocked
//!      ^                                          |                                  |
//!      |        sync request   -> TS_E_SYNCHRONOUS (call S_OK, hrSession = error)     |
//!      |        async request  -> queued, TS_S_ASYNC                                 |
//!      +-----------------------------------------------------------------------------+
//!                 after release: flush the composition update, the deferred change
//!                 notifications, then grant the queued lock
//! ```
//!
//! * One lock at a time (single-threaded apartment). Unlocked plus a sync or async request: the
//!   lock is granted immediately and `OnLockGranted` runs inside `RequestLock`; its HRESULT is
//!   the `hrSession` result.
//! * Methods that read need a read lock, methods that mutate a read/write lock, else
//!   `TS_E_NOLOCK`.
//! * Notifications (`OnTextChange`, `OnSelectionChange`, `OnLayoutChange`, `OnStatusChange`) are
//!   only sent while unlocked. Host edits made while a lock is held are deferred, text changes
//!   merged into one covering change, and flushed after the release before a queued lock is
//!   granted.
//! * Edits made by TSF through this interface are not echoed back as notifications (TSF adjusts
//!   its anchors from the returned `TS_TEXTCHANGE`; echoing would apply them twice). The host
//!   calls our change notifications from inside its own `replace`/`set_selection`/`ime`
//!   implementations; calls made while we are executing such a host call are ignored (`echo`).
//! * No `RefCell` borrow is held across a call into the host or into TSF: both re-enter
//!   (`RequestLock` from inside `OnTextChange`, `GetText` from inside `OnLockGranted`, host
//!   notifications from inside `replace`).
//! * Composition updates reach the host only after the lock is released, so the display
//!   attributes TSF reports at the end of the edit are included, and no message has to be
//!   posted to refresh them (a refresh that schedules itself livelocks the message loop and
//!   starves input: posted messages outrank input in `GetMessage`).

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use ssw_core::edit::Selection as HostSelection;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{ImeAttrStyle, ImeEvent};
use windows::Win32::Foundation::{E_INVALIDARG, E_UNEXPECTED, HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Ole::{CONNECT_E_ADVISELIMIT, CONNECT_E_NOCONNECTION};
use windows::Win32::UI::TextServices::{
    GUID_PROP_INPUTSCOPE, ITextStoreACPSink, TEXT_STORE_LOCK_FLAGS, TEXT_STORE_TEXT_CHANGE_FLAGS,
    TS_AS_LAYOUT_CHANGE, TS_AS_SEL_CHANGE, TS_AS_STATUS_CHANGE, TS_AS_TEXT_CHANGE,
    TS_DEFAULT_SELECTION, TS_E_INVALIDPOINT, TS_E_INVALIDPOS, TS_E_NOLAYOUT, TS_E_NOLOCK,
    TS_E_NOSELECTION, TS_E_READONLY, TS_E_SYNCHRONOUS, TS_IAS_QUERYONLY, TS_LC_CHANGE,
    TS_LC_CREATE, TS_LC_DESTROY, TS_LF_READ, TS_LF_READWRITE, TS_LF_SYNC, TS_S_ASYNC,
    TS_SD_READONLY, TS_SS_NOHIDDENTEXT, TS_STATUS, TS_TEXTCHANGE, TsLayoutCode,
};
use windows::core::{Error, GUID, HRESULT, Result};

use super::TextStoreHost;
use super::view::{AcpSelection, Part, Target, TextChange, View, merge_changes};
use crate::dpi;

/// How strongly the document is locked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum LockKind {
    None,
    Read,
    ReadWrite,
}

impl LockKind {
    /// The flags `OnLockGranted` receives (`TS_LF_READ` = 2, `TS_LF_READWRITE` = 6).
    fn flags(self) -> TEXT_STORE_LOCK_FLAGS {
        match self {
            LockKind::None => TEXT_STORE_LOCK_FLAGS(0),
            LockKind::Read => TS_LF_READ,
            LockKind::ReadWrite => TS_LF_READWRITE,
        }
    }

    /// Parses `RequestLock` flags, ignoring `TS_LF_SYNC`. `None` for an invalid combination.
    fn from_request(flags: u32) -> Option<LockKind> {
        match flags & TS_LF_READWRITE.0 {
            6 => Some(LockKind::ReadWrite),
            2 => Some(LockKind::Read),
            _ => None,
        }
    }
}

/// Window DIPs and screen pixels.
#[derive(Debug, Clone, Copy)]
struct Screen {
    hwnd: HWND,
}

impl Screen {
    fn scale(self) -> f32 {
        dpi::scale_for_dpi(dpi::window_dpi(self.hwnd))
    }

    /// A window DIP rectangle as a screen pixel rectangle (rounded outwards).
    fn to_screen(self, rect: Rect) -> RECT {
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: `origin` is a valid POINT; a failing call leaves it at (0, 0).
        let _ = unsafe { ClientToScreen(self.hwnd, &mut origin) };
        let scale = self.scale();
        RECT {
            left: origin.x + (rect.left() * scale).floor() as i32,
            top: origin.y + (rect.top() * scale).floor() as i32,
            right: origin.x + (rect.right() * scale).ceil() as i32,
            bottom: origin.y + (rect.bottom() * scale).ceil() as i32,
        }
    }

    /// A screen pixel position as window DIPs.
    fn to_dip(self, mut point: POINT) -> Point {
        // SAFETY: `point` is a valid POINT; a failing call leaves it in screen coordinates.
        let _ = unsafe { ScreenToClient(self.hwnd, &mut point) };
        let scale = self.scale();
        Point::new(point.x as f32 / scale, point.y as f32 / scale)
    }
}

/// The sink TSF gave us with `AdviseSink`.
struct Advised {
    sink: ITextStoreACPSink,
    /// COM identity (the `IUnknown` pointer) of the sink.
    id: usize,
    mask: u32,
}

/// Notifications waiting for the document to be unlocked.
#[derive(Default)]
struct Pending {
    text: Option<TextChange>,
    selection: bool,
    layout: Option<i32>,
    status: bool,
}

/// What `GetText` returns.
#[derive(Debug)]
pub(super) struct TextOut {
    pub(super) text: Vec<u16>,
    pub(super) run_count: u32,
    /// The position after the returned text.
    pub(super) next: usize,
}

/// What `InsertTextAtSelection` returns.
#[derive(Debug)]
pub(super) struct InsertOut {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) change: Option<TextChange>,
}

/// Keeps host calls from being echoed back to TSF as change notifications.
struct Echo<'a>(&'a Cell<u32>);

impl<'a> Echo<'a> {
    fn enter(depth: &'a Cell<u32>) -> Self {
        depth.set(depth.get() + 1);
        Self(depth)
    }
}

impl Drop for Echo<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

fn hr_of(result: Result<()>) -> HRESULT {
    match result {
        Ok(()) => HRESULT(0),
        Err(error) => error.code(),
    }
}

fn error(code: HRESULT) -> Error {
    Error::from_hresult(code)
}

/// The state behind one `ITextStoreACP2`.
pub(super) struct Store {
    host: Rc<dyn TextStoreHost>,
    screen: Screen,
    sink: RefCell<Option<Advised>>,
    lock: Cell<LockKind>,
    queued: Cell<LockKind>,
    pending: RefCell<Pending>,
    view: RefCell<View>,
    echo: Cell<u32>,
    input_scope_wanted: Cell<bool>,
}

impl Store {
    pub(super) fn new(hwnd: HWND, host: Rc<dyn TextStoreHost>) -> Self {
        Self {
            host,
            screen: Screen { hwnd },
            sink: RefCell::new(None),
            lock: Cell::new(LockKind::None),
            queued: Cell::new(LockKind::None),
            pending: RefCell::new(Pending::default()),
            view: RefCell::new(View::new()),
            echo: Cell::new(0),
            input_scope_wanted: Cell::new(false),
        }
    }

    /// Runs `f` on the up-to-date view. `f` must not call the host or TSF.
    fn with_view<R>(&self, f: impl FnOnce(&mut View) -> R) -> R {
        if !self.view.borrow().is_valid() {
            let text = self.host.text();
            self.view.borrow_mut().rebuild(&text);
        }
        f(&mut self.view.borrow_mut())
    }

    /// Runs a host call whose change notifications TSF must not see.
    fn echoing<R>(&self, f: impl FnOnce() -> R) -> R {
        let _echo = Echo::enter(&self.echo);
        f()
    }

    pub(super) fn is_composing(&self) -> bool {
        self.view.borrow().composition().is_some()
    }

    pub(super) fn has_sink(&self) -> bool {
        self.sink.borrow().is_some()
    }

    fn need(&self, kind: LockKind) -> Result<()> {
        if self.lock.get() >= kind {
            Ok(())
        } else {
            Err(error(TS_E_NOLOCK))
        }
    }

    // ---- sink ------------------------------------------------------------------------------

    pub(super) fn advise_sink(&self, sink: ITextStoreACPSink, id: usize, mask: u32) -> Result<()> {
        let mut slot = self.sink.borrow_mut();
        match slot.as_mut() {
            Some(advised) if advised.id == id => {
                advised.mask = mask;
                Ok(())
            }
            Some(_) => Err(error(CONNECT_E_ADVISELIMIT)),
            None => {
                *slot = Some(Advised { sink, id, mask });
                Ok(())
            }
        }
    }

    pub(super) fn unadvise_sink(&self, id: usize) -> Result<()> {
        let mut slot = self.sink.borrow_mut();
        match slot.as_ref() {
            Some(advised) if advised.id == id => {
                *slot = None;
                Ok(())
            }
            _ => Err(error(CONNECT_E_NOCONNECTION)),
        }
    }

    // ---- locks -----------------------------------------------------------------------------

    /// `ITextStoreACP2::RequestLock`. The returned HRESULT is `*phrSession`; the call is S_OK.
    pub(super) fn request_lock(&self, flags: u32) -> Result<HRESULT> {
        let sync = flags & TS_LF_SYNC != 0;
        let Some(kind) = LockKind::from_request(flags) else {
            return Err(error(E_INVALIDARG));
        };
        if !self.has_sink() {
            return Err(error(E_UNEXPECTED));
        }
        if self.lock.get() != LockKind::None {
            if sync {
                return Ok(TS_E_SYNCHRONOUS);
            }
            // Async while locked (including a read to read/write upgrade): remember the
            // strongest request and grant it after the release.
            self.queued.set(self.queued.get().max(kind));
            return Ok(TS_S_ASYNC);
        }
        Ok(self.grant(kind))
    }

    fn grant(&self, kind: LockKind) -> HRESULT {
        let sink = self
            .sink
            .borrow()
            .as_ref()
            .map(|advised| advised.sink.clone());
        let Some(sink) = sink else {
            return E_UNEXPECTED;
        };
        self.lock.set(kind);
        // SAFETY: `sink` is TSF's live sink interface; `OnLockGranted` takes only flags.
        let hr = hr_of(unsafe { sink.OnLockGranted(kind.flags()) });
        self.lock.set(LockKind::None);
        self.flush_composition();
        self.after_unlock();
        hr
    }

    fn after_unlock(&self) {
        self.flush_notifications();
        // The document is unlocked here: a sink callback in `flush_notifications` that requests
        // a lock gets it granted and released inside the request, and drains the queue itself.
        let queued = self.queued.replace(LockKind::None);
        if queued != LockKind::None {
            let _ = self.grant(queued);
        }
    }

    // ---- notifications ---------------------------------------------------------------------

    fn queue_text_change(&self, change: TextChange) {
        let mut pending = self.pending.borrow_mut();
        pending.text = Some(match pending.text {
            Some(previous) => merge_changes(previous, change),
            None => change,
        });
        pending.layout.get_or_insert(TS_LC_CHANGE.0);
    }

    /// Sends what is pending, if (and only if) the document is unlocked.
    fn flush_notifications(&self) {
        if self.lock.get() != LockKind::None {
            return;
        }
        let (text, selection, layout, status) = {
            let mut pending = self.pending.borrow_mut();
            (
                pending.text.take(),
                std::mem::take(&mut pending.selection),
                pending.layout.take(),
                std::mem::take(&mut pending.status),
            )
        };
        let advised = self
            .sink
            .borrow()
            .as_ref()
            .map(|advised| (advised.sink.clone(), advised.mask));
        let Some((sink, mask)) = advised else {
            return;
        };
        if let Some(change) = text
            && mask & TS_AS_TEXT_CHANGE != 0
        {
            let change = TS_TEXTCHANGE {
                acpStart: change.start as i32,
                acpOldEnd: change.old_end as i32,
                acpNewEnd: change.new_end as i32,
            };
            // SAFETY: `change` outlives the call; `sink` is TSF's live sink interface.
            let _ = unsafe { sink.OnTextChange(TEXT_STORE_TEXT_CHANGE_FLAGS(0), &change) };
        }
        if selection && mask & TS_AS_SEL_CHANGE != 0 {
            // SAFETY: plain call on TSF's live sink interface.
            let _ = unsafe { sink.OnSelectionChange() };
        }
        if let Some(code) = layout
            && mask & TS_AS_LAYOUT_CHANGE != 0
        {
            // SAFETY: plain call on TSF's live sink interface; the view cookie is ours (0).
            let _ = unsafe { sink.OnLayoutChange(TsLayoutCode(code), 0) };
        }
        if status && mask & TS_AS_STATUS_CHANGE != 0 {
            // SAFETY: plain call on TSF's live sink interface.
            let _ = unsafe { sink.OnStatusChange(self.status().dwDynamicFlags) };
        }
    }

    // ---- notifications from the host ---------------------------------------------------------

    /// The host replaced the bytes `start..start + removed.len()` (`removed` is their old text)
    /// by `inserted`.
    pub(super) fn text_changed(&self, start: usize, removed: &str, inserted: &str) {
        if self.echo.get() > 0 {
            // A TSF-initiated edit running inside the host: TSF already knows it and the store
            // applies it to its own cache.
            return;
        }
        self.view
            .borrow_mut()
            .apply_host_edit(start, removed.len(), inserted);
        if !self.has_sink() {
            return;
        }
        let first = self.with_view(|view| view.byte_to_acp(start, true));
        let change = TextChange {
            start: first,
            old_end: first + removed.encode_utf16().count(),
            new_end: first + inserted.encode_utf16().count(),
        };
        self.view.borrow_mut().adjust_selection(change);
        self.queue_text_change(change);
        self.pending.borrow_mut().selection = true;
        self.flush_notifications();
    }

    /// The host moved its selection. Ignored while composing: TSF owns the selection then.
    pub(super) fn selection_changed(&self) {
        if self.echo.get() > 0 || self.is_composing() {
            return;
        }
        self.pending.borrow_mut().selection = true;
        self.flush_notifications();
    }

    /// Text rectangles changed (scroll, resize, zoom, font, DPI).
    pub(super) fn layout_changed(&self) {
        self.pending
            .borrow_mut()
            .layout
            .get_or_insert(TS_LC_CHANGE.0);
        self.flush_notifications();
    }

    /// The view exists and can answer layout queries.
    pub(super) fn layout_created(&self) {
        self.pending.borrow_mut().layout = Some(TS_LC_CREATE.0);
        self.flush_notifications();
    }

    /// The view is going away.
    pub(super) fn layout_destroyed(&self) {
        self.pending.borrow_mut().layout = Some(TS_LC_DESTROY.0);
        self.flush_notifications();
    }

    /// The read-only state changed.
    pub(super) fn status_changed(&self) {
        self.pending.borrow_mut().status = true;
        self.flush_notifications();
    }

    // ---- ITextStoreACP2 --------------------------------------------------------------------

    pub(super) fn status(&self) -> TS_STATUS {
        TS_STATUS {
            dwDynamicFlags: if self.host.is_read_only() {
                TS_SD_READONLY
            } else {
                0
            },
            dwStaticFlags: TS_SS_NOHIDDENTEXT,
        }
    }

    /// The range an insertion over `start..end` would replace, snapped to scalar boundaries.
    pub(super) fn query_insert(&self, start: i32, end: i32) -> (usize, usize) {
        self.with_view(|view| {
            let len = view.len();
            let start = (start.max(0) as usize).min(len);
            let end = (end.max(0) as usize).min(len).max(start);
            view.snap_range(start, end)
        })
    }

    /// The selection TSF sees: the composition's while composing, else the host's.
    fn current_selection(&self) -> AcpSelection {
        let host = self.host.selection();
        self.with_view(|view| view.selection(host))
    }

    pub(super) fn get_selection(&self, index: u32) -> Result<AcpSelection> {
        self.need(LockKind::Read)?;
        if index != 0 && index != TS_DEFAULT_SELECTION {
            return Err(error(TS_E_NOSELECTION));
        }
        Ok(self.current_selection())
    }

    pub(super) fn set_selection(&self, selections: &[AcpSelection]) -> Result<()> {
        self.need(LockKind::ReadWrite)?;
        let [selection] = selections else {
            // TS_SS_DISJOINTSEL is not advertised: exactly one selection.
            return Err(error(E_INVALIDARG));
        };
        let target = self.with_view(|view| {
            if selection.start > selection.end || selection.end > view.len() {
                return Err(error(TS_E_INVALIDPOS));
            }
            let (start, end) = view.snap_range(selection.start, selection.end);
            let snapped = AcpSelection {
                start,
                end,
                backward: selection.backward,
            };
            if let Some(comp) = view.composition_mut() {
                comp.selection = snapped;
                return Ok(None);
            }
            let (a, b) = (view.acp_to_byte(start), view.acp_to_byte(end));
            Ok(Some(if snapped.backward {
                HostSelection::new(b, a)
            } else {
                HostSelection::new(a, b)
            }))
        })?;
        if let Some(target) = target {
            self.echoing(|| self.host.set_selection(target));
        }
        Ok(())
    }

    /// `end == -1` means "to the end of the document".
    pub(super) fn get_text(
        &self,
        start: i32,
        end: i32,
        max_chars: usize,
        max_runs: usize,
    ) -> Result<TextOut> {
        self.need(LockKind::Read)?;
        self.with_view(|view| {
            let (start, end) = Self::range_args(view, start, end)?;
            let mut take = (end - start).min(max_chars);
            // Never cut a surrogate pair in half because the caller's buffer is full.
            if start + take < end && take > 0 && !view.is_boundary(start + take) {
                take -= 1;
            }
            Ok(TextOut {
                text: view.slice(start..start + take),
                run_count: u32::from(max_runs > 0 && take > 0),
                next: start + take,
            })
        })
    }

    pub(super) fn end_acp(&self) -> Result<usize> {
        self.need(LockKind::Read)?;
        Ok(self.with_view(|view| view.len()))
    }

    /// Validates `start..end` (`end == -1`: to the end) and snaps it to scalar boundaries.
    fn range_args(view: &View, start: i32, end: i32) -> Result<(usize, usize)> {
        let len = view.len();
        let end = match end {
            -1 => len,
            end if end < 0 => return Err(error(TS_E_INVALIDPOS)),
            end => end as usize,
        };
        if start < 0 || start as usize > end || end > len {
            return Err(error(TS_E_INVALIDPOS));
        }
        Ok(view.snap_range(start as usize, end))
    }

    pub(super) fn set_text(&self, start: i32, end: i32, text: &[u16]) -> Result<TextChange> {
        self.need(LockKind::ReadWrite)?;
        if self.host.is_read_only() {
            return Err(error(TS_E_READONLY));
        }
        let new = String::from_utf16(text).map_err(|_| error(E_INVALIDARG))?;
        let (start, end) = self.with_view(|view| Self::range_args(view, start, end))?;
        self.replace_acp(start, end, text, &new)
    }

    /// Replaces the scalar-aligned ACP range `start..end` by `text` (`new` is its UTF-8 form).
    fn replace_acp(&self, start: usize, end: usize, text: &[u16], new: &str) -> Result<TextChange> {
        let target = self.with_view(|view| view.classify(start, end));
        let change = match target {
            Target::Composition(range) => self.with_view(|view| view.edit_composition(range, text)),
            Target::Host(bytes) => {
                if !self.echoing(|| self.host.replace(bytes.clone(), new)) {
                    return Err(error(TS_E_READONLY));
                }
                self.view
                    .borrow_mut()
                    .apply_host_edit(bytes.start, bytes.len(), new);
                TextChange {
                    start,
                    old_end: end,
                    new_end: start + text.len(),
                }
            }
            Target::Straddles => return Err(error(TS_E_INVALIDPOS)),
        };
        self.pending
            .borrow_mut()
            .layout
            .get_or_insert(TS_LC_CHANGE.0);
        Ok(change)
    }

    pub(super) fn insert_text_at_selection(&self, flags: u32, text: &[u16]) -> Result<InsertOut> {
        let query_only = flags & TS_IAS_QUERYONLY != 0;
        self.need(if query_only {
            LockKind::Read
        } else {
            LockKind::ReadWrite
        })?;
        let selection = self.current_selection();
        if query_only {
            return Ok(InsertOut {
                start: selection.start,
                end: selection.start + text.len(),
                change: None,
            });
        }
        if self.host.is_read_only() {
            return Err(error(TS_E_READONLY));
        }
        let new = String::from_utf16(text).map_err(|_| error(E_INVALIDARG))?;
        let change = self.replace_acp(selection.start, selection.end, text, &new)?;
        // The caret goes behind the inserted text.
        let caret = self.with_view(|view| {
            if let Some(comp) = view.composition_mut() {
                comp.selection = AcpSelection::caret(change.new_end);
                None
            } else {
                Some(view.acp_to_byte(change.new_end))
            }
        });
        if let Some(byte) = caret {
            self.echoing(|| self.host.set_selection(HostSelection::caret(byte)));
        }
        Ok(InsertOut {
            start: change.start,
            end: change.new_end,
            change: Some(change),
        })
    }

    /// Screen rectangle of `start..end` and whether the view clips it.
    pub(super) fn text_ext(&self, view_cookie: u32, start: i32, end: i32) -> Result<(RECT, bool)> {
        self.need(LockKind::Read)?;
        if view_cookie != 0 {
            return Err(error(E_INVALIDARG));
        }
        let parts = self.with_view(|view| {
            let len = view.len();
            if start < 0 || end < start || end as usize > len {
                return Err(error(TS_E_INVALIDPOS));
            }
            Ok(view.parts(start as usize, end as usize))
        })?;
        let mut bounds: Option<Rect> = None;
        for part in parts {
            let rect = match part {
                Part::Host(bytes) => self.host.range_rect(bytes),
                Part::Composition(range) => self.host.composition_rect(range),
            };
            bounds = match (bounds, rect) {
                (Some(known), Some(rect)) => Some(known.union(rect)),
                (known, rect) => known.or(rect),
            };
        }
        let (Some(bounds), Some(visible)) = (bounds, self.host.view_rect()) else {
            return Err(error(TS_E_NOLAYOUT));
        };
        Ok((
            self.screen.to_screen(bounds),
            !visible.contains_rect(bounds),
        ))
    }

    /// Screen rectangle of the visible text area.
    pub(super) fn screen_ext(&self, view_cookie: u32) -> Result<RECT> {
        if view_cookie != 0 {
            return Err(error(E_INVALIDARG));
        }
        let rect = self.host.view_rect().ok_or_else(|| error(TS_E_NOLAYOUT))?;
        Ok(self.screen.to_screen(rect))
    }

    /// The ACP at a screen position.
    pub(super) fn acp_from_point(&self, view_cookie: u32, point: POINT) -> Result<usize> {
        if view_cookie != 0 {
            return Err(error(E_INVALIDARG));
        }
        let byte = self
            .host
            .offset_at_point(self.screen.to_dip(point))
            .ok_or_else(|| error(TS_E_INVALIDPOINT))?;
        Ok(self.with_view(|view| view.byte_to_acp(byte, false)))
    }

    /// `RequestSupportedAttrs` / `RequestAttrsAtPosition`: the only attribute we serve is the
    /// input scope (plain text), requested explicitly or with an empty filter (= all).
    pub(super) fn request_attrs(&self, filter: &[GUID]) {
        if filter.is_empty() || filter.contains(&GUID_PROP_INPUTSCOPE) {
            self.input_scope_wanted.set(true);
        }
    }

    /// True once per request: `RetrieveRequestedAttrs` must hand out the input scope.
    pub(super) fn take_input_scope_request(&self) -> bool {
        self.input_scope_wanted.replace(false)
    }

    // ---- composition -----------------------------------------------------------------------

    /// `OnStartComposition`: `range` is the composition's extent (or `None` if TSF gave none).
    /// Returns whether the composition is accepted.
    pub(super) fn composition_started(&self, range: Option<Range<usize>>) -> bool {
        if self.is_composing() || self.host.is_read_only() {
            return false;
        }
        let selection = self.current_selection();
        let (start, end) = range.map_or((selection.start, selection.end), |r| (r.start, r.end));
        let (bytes, update) = self.with_view(|view| {
            let (start, end) = view.snap_range(start, end.max(start));
            let bytes = view.begin_composition(start, end, selection);
            // A composition over existing text starts out displaying that text.
            let update = if end > start {
                view.update_event()
            } else {
                None
            };
            (bytes, update)
        });
        self.echoing(|| {
            self.host
                .set_selection(HostSelection::new(bytes.start, bytes.end));
            self.host.ime(ImeEvent::Start);
            if let Some(update) = update {
                self.host.ime(update);
            }
        });
        // The host removed the text the composition replaces.
        if !bytes.is_empty() {
            self.view
                .borrow_mut()
                .apply_host_edit(bytes.start, bytes.len(), "");
        }
        true
    }

    /// `OnUpdateComposition`: `range` is the new extent, `None` when only the content changed.
    pub(super) fn composition_updated(&self, range: Option<Range<usize>>) {
        let Some(current) = self.with_view(|view| view.composition_extent()) else {
            return;
        };
        match range {
            Some(range) if range != current => {
                // The extent moved (an IME committed a clause and shifted the start): flatten the
                // old composition into the host text and compose over the new extent.
                let selection = self.finish_composition();
                self.composition_started(Some(range));
                self.with_view(|view| {
                    if let (Some(comp), Some(selection)) = (view.composition_mut(), selection) {
                        comp.selection = selection;
                        comp.dirty = true;
                    }
                });
            }
            _ => self.with_view(|view| {
                if let Some(comp) = view.composition_mut() {
                    comp.dirty = true;
                }
            }),
        }
    }

    /// `OnEndComposition`: commits the string to the host and ends the composition.
    pub(super) fn composition_ended(&self) {
        let Some(selection) = self.finish_composition() else {
            return;
        };
        let target = self.with_view(|view| {
            let (a, b) = (
                view.acp_to_byte(selection.start),
                view.acp_to_byte(selection.end),
            );
            if selection.backward {
                HostSelection::new(b, a)
            } else {
                HostSelection::new(a, b)
            }
        });
        if self.host.selection() != target {
            self.echoing(|| self.host.set_selection(target));
        }
    }

    /// Commits the composition string to the host and ends the composition. Returns TSF's
    /// selection (ACPs), which is the same in the host text now.
    fn finish_composition(&self) -> Option<AcpSelection> {
        let comp = self.view.borrow_mut().take_composition()?;
        let text = String::from_utf16_lossy(&comp.units);
        self.echoing(|| {
            self.host.set_selection(HostSelection::caret(comp.host_at));
            if !text.is_empty() {
                self.host.ime(ImeEvent::Commit(text.clone()));
            }
            self.host.ime(ImeEvent::End);
        });
        if !text.is_empty() {
            self.view
                .borrow_mut()
                .apply_host_edit(comp.host_at, 0, &text);
        }
        Some(comp.selection)
    }

    /// The display attributes TSF reports for the composition (ACP runs); ignored when no
    /// composition is active.
    pub(super) fn display_attributes(&self, runs: &[(Range<usize>, ImeAttrStyle)]) {
        self.with_view(|view| view.set_attrs(runs));
    }

    /// Sends the composition string, caret and attributes to the host if they changed. The view
    /// is valid whenever a composition exists, and `take_update` is `None` without one.
    fn flush_composition(&self) {
        let update = self.view.borrow_mut().take_update();
        if let Some(update) = update {
            self.echoing(|| self.host.ime(update));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_FAIL;

    #[test]
    fn lock_kinds_map_to_the_flags_on_lock_granted_receives() {
        assert_eq!(LockKind::None.flags().0, 0);
        assert_eq!(LockKind::Read.flags().0, 2);
        assert_eq!(LockKind::ReadWrite.flags().0, 6);
    }

    #[test]
    fn a_session_result_is_the_error_code_or_zero() {
        assert_eq!(hr_of(Ok(())), HRESULT(0));
        assert_eq!(hr_of(Err(error(E_FAIL))), E_FAIL);
    }
}
