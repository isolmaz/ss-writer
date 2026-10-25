//! The TSF text store against real COM objects, in process, with a hidden window that is never
//! shown, activated or focused.
//!
//! Two layers:
//!
//! - `direct_*`: the store's `ITextStoreACP2` driven through its COM interface by a scripted
//!   `ITextStoreACPSink` (what TSF's own sink does), with a host that edits a real string. The
//!   scripts run inside `OnLockGranted`, i.e. while the document is locked, exactly like TSF's
//!   edit sessions. Results are logged and asserted after the call: a panic inside a COM
//!   callback would abort the process.
//! - `tsf_*`: the real thread manager (`msctf.dll`) drives the store through edit sessions and
//!   composition calls, as dictation, the emoji panel and IMEs do. Real IME composition, live
//!   dictation and the emoji panel are manual checks (`docs/manual-test.md`).

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::ops::Range;
use std::rc::Rc;

use ssw_core::edit::Selection;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{ImeAttr, ImeAttrStyle, ImeEvent};
use ssw_win::com::ComGuard;
use ssw_win::dpi;
use ssw_win::tsf::{TextServices, TextStore, TextStoreHost};
use windows::Win32::Foundation::{
    CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_POINTER, E_UNEXPECTED,
    HWND, POINT, RECT,
};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoRegisterClassObject, CoRevokeClassObject,
    CoTaskMemFree, IClassFactory, IClassFactory_Impl, REGCLS_MULTIPLEUSE,
};
use windows::Win32::System::Ole::{
    CONNECT_E_ADVISELIMIT, CONNECT_E_CANNOTCONNECT, CONNECT_E_NOCONNECTION,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::TextServices::{
    CLSID_TF_CategoryMgr, CLSID_TF_DisplayAttributeMgr, GUID_PROP_ATTRIBUTE, GUID_PROP_INPUTSCOPE,
    IEnumTfDisplayAttributeInfo, INSERT_TEXT_AT_SELECTION_FLAGS, IS_DEFAULT, ITextStoreACP2,
    ITextStoreACPSink, ITextStoreACPSink_Impl, ITfCategoryMgr, ITfComposition, ITfCompositionSink,
    ITfCompositionSink_Impl, ITfCompositionView, ITfContext, ITfContextComposition,
    ITfContextOwnerCompositionSink, ITfDisplayAttributeInfo, ITfDisplayAttributeInfo_Impl,
    ITfDisplayAttributeMgr, ITfDisplayAttributeMgr_Impl, ITfEditRecord, ITfInputScope,
    ITfInsertAtSelection, ITfRange, ITfRangeACP, ITfTextEditSink, InputScope,
    TEXT_STORE_LOCK_FLAGS, TEXT_STORE_TEXT_CHANGE_FLAGS, TF_AE_END, TF_ANCHOR_END, TF_ANCHOR_START,
    TF_ATTR_CONVERTED, TF_ATTR_FIXEDCONVERTED, TF_ATTR_INPUT, TF_ATTR_INPUT_ERROR,
    TF_ATTR_TARGET_CONVERTED, TF_ATTR_TARGET_NOTCONVERTED, TF_CONTEXT_EDIT_CONTEXT_FLAGS,
    TF_DA_ATTR_INFO, TF_DISPLAYATTRIBUTE, TF_ES_READ, TF_ES_READWRITE, TF_ES_SYNC, TF_SELECTION,
    TF_SELECTIONSTYLE, TS_AE_END, TS_AE_START, TS_AS_LAYOUT_CHANGE, TS_AS_SEL_CHANGE,
    TS_AS_STATUS_CHANGE, TS_AS_TEXT_CHANGE, TS_ATTR_FIND_WANT_VALUE, TS_ATTRVAL,
    TS_DEFAULT_SELECTION, TS_E_INVALIDPOINT, TS_E_INVALIDPOS, TS_E_NOLAYOUT, TS_E_NOLOCK,
    TS_E_NOSELECTION, TS_E_READONLY, TS_E_SYNCHRONOUS, TS_IAS_QUERYONLY, TS_LF_READ,
    TS_LF_READWRITE, TS_LF_SYNC, TS_RUNINFO, TS_S_ASYNC, TS_SD_READONLY, TS_SELECTION_ACP,
    TS_SELECTIONSTYLE, TS_SS_NOHIDDENTEXT, TS_TEXTCHANGE, TsLayoutCode,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
    TranslateMessage, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, BSTR, GUID, HRESULT, IUnknown, Interface, OutRef, Ref, implement, w};

/// A COM call on a live interface owned by the test, with arguments that outlive the call.
macro_rules! com {
    ($call:expr) => {
        // SAFETY: a COM call on a live interface pointer owned by the test; arguments are valid
        // for the duration of the call.
        unsafe { $call }
    };
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn code<T>(result: windows::core::Result<T>) -> HRESULT {
    result.err().expect("the call must fail").code()
}

// ---- the hidden window -------------------------------------------------------------------------

/// A window that is created and never shown, activated or focused: a tool window of the system
/// `STATIC` class at (-32000, -32000), outside every monitor.
struct HiddenWindow(HWND);

impl HiddenWindow {
    fn new() -> Self {
        // SAFETY: creates a window of a system class on this thread; it is never shown.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_POPUP,
                -32000,
                -32000,
                640,
                480,
                None,
                None,
                None,
                None,
            )
        }
        .expect("the hidden window is created");
        Self(hwnd)
    }

    /// Where the test host's DIP rectangle `rect` lies on the screen, from the real window
    /// origin and DPI.
    fn screen_rect(&self, rect: Rect) -> RECT {
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: `origin` is a valid POINT.
        let _ = unsafe { ClientToScreen(self.0, &mut origin) };
        let scale = dpi::scale_for_dpi(dpi::window_dpi(self.0));
        RECT {
            left: origin.x + (rect.left() * scale).floor() as i32,
            top: origin.y + (rect.top() * scale).floor() as i32,
            right: origin.x + (rect.right() * scale).ceil() as i32,
            bottom: origin.y + (rect.bottom() * scale).ceil() as i32,
        }
    }

    /// The screen pixel of a window DIP point.
    fn screen_point(&self, x: f32, y: f32) -> POINT {
        let mut origin = POINT { x: 0, y: 0 };
        // SAFETY: `origin` is a valid POINT.
        let _ = unsafe { ClientToScreen(self.0, &mut origin) };
        let scale = dpi::scale_for_dpi(dpi::window_dpi(self.0));
        POINT {
            x: origin.x + (x * scale).round() as i32,
            y: origin.y + (y * scale).round() as i32,
        }
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window this struct created on this thread.
        let _ = unsafe { DestroyWindow(self.0) };
    }
}

// ---- the host ----------------------------------------------------------------------------------

const CELL_W: f32 = 10.0;
const CELL_H: f32 = 20.0;
const ORIGIN: (f32, f32) = (4.0, 6.0);

/// What the host tells the store (the wiring a real editor does after its own edits).
#[derive(Debug)]
enum Note {
    Text {
        start: usize,
        removed: String,
        inserted: String,
    },
    Selection,
}

struct HostState {
    text: String,
    selection: Selection,
    read_only: bool,
    view: Option<Rect>,
    overlay: Option<String>,
    events: Vec<ImeEvent>,
    replaces: Vec<(Range<usize>, String)>,
    refuse: bool,
}

/// An editor over a `String` laid out on a grid of 10 x 20 DIP cells (one cell per char,
/// text origin (4, 6)). It implements the IME overlay semantics of the widget kit and calls the
/// store's notifications synchronously from inside its own edit methods, the worst case for
/// re-entrancy.
struct TestHost {
    state: RefCell<HostState>,
    notify: RefCell<Option<Notify>>,
}

/// What the test host calls after its own edits.
type Notify = Box<dyn Fn(Note)>;

impl TestHost {
    fn new(text: &str) -> Rc<Self> {
        Rc::new(Self {
            state: RefCell::new(HostState {
                text: text.to_owned(),
                selection: Selection::caret(text.len()),
                read_only: false,
                view: Some(Rect::new(0.0, 0.0, 300.0, 100.0)),
                overlay: None,
                events: Vec::new(),
                replaces: Vec::new(),
                refuse: false,
            }),
            notify: RefCell::new(None),
        })
    }

    fn link(&self, notify: impl Fn(Note) + 'static) {
        *self.notify.borrow_mut() = Some(Box::new(notify));
    }

    fn tell(&self, note: Note) {
        // The state borrow is released: the store may call back into the host.
        if let Some(notify) = self.notify.borrow().as_ref() {
            notify(note);
        }
    }

    fn doc(&self) -> String {
        self.state.borrow().text.clone()
    }

    fn selection_now(&self) -> Selection {
        self.state.borrow().selection
    }

    fn overlay_now(&self) -> Option<String> {
        self.state.borrow().overlay.clone()
    }

    fn events_now(&self) -> Vec<ImeEvent> {
        self.state.borrow().events.clone()
    }

    fn set_read_only(&self, read_only: bool) {
        self.state.borrow_mut().read_only = read_only;
    }

    fn set_view(&self, view: Option<Rect>) {
        self.state.borrow_mut().view = view;
    }

    fn set_refuse(&self, refuse: bool) {
        self.state.borrow_mut().refuse = refuse;
    }

    /// Replaces `range`, mapping the selection through the edit; returns the removed text.
    fn splice(&self, range: Range<usize>, text: &str) -> String {
        let mut state = self.state.borrow_mut();
        let removed = state.text[range.clone()].to_owned();
        state.text.replace_range(range.clone(), text);
        let delta = text.len() as isize - removed.len() as isize;
        let map = |offset: usize| {
            if offset <= range.start {
                offset
            } else if offset >= range.end {
                (offset as isize + delta) as usize
            } else {
                range.start + text.len()
            }
        };
        let selection = state.selection;
        state.selection = Selection::new(map(selection.anchor), map(selection.head));
        removed
    }

    /// A user edit (typing): replaces `range` by `text`, caret after it, and tells the store.
    fn edit(&self, range: Range<usize>, text: &str) {
        let start = range.start;
        let removed = self.splice(range, text);
        self.state.borrow_mut().selection = Selection::caret(start + text.len());
        self.tell(Note::Text {
            start,
            removed,
            inserted: text.to_owned(),
        });
    }

    /// The user moved the caret and the host tells the store.
    fn move_caret(&self, selection: Selection) {
        self.state.borrow_mut().selection = selection;
        self.tell(Note::Selection);
    }

    /// Shows another document, caret at its end, without telling the store: what a tab switch
    /// does to the editor.
    fn switch_to(&self, text: &str) {
        let mut state = self.state.borrow_mut();
        state.text = text.to_owned();
        state.selection = Selection::caret(text.len());
    }

    fn line_col(text: &str, byte: usize) -> (usize, usize) {
        let before = &text[..byte];
        let line = before.matches('\n').count();
        let col = before.rsplit('\n').next().unwrap_or("").chars().count();
        (line, col)
    }

    fn x(col: usize) -> f32 {
        ORIGIN.0 + col as f32 * CELL_W
    }

    fn y(line: usize) -> f32 {
        ORIGIN.1 + line as f32 * CELL_H
    }
}

impl TextStoreHost for TestHost {
    fn text(&self) -> String {
        self.doc()
    }

    fn selection(&self) -> Selection {
        self.selection_now()
    }

    fn set_selection(&self, selection: Selection) {
        self.state.borrow_mut().selection = selection;
        self.tell(Note::Selection);
    }

    fn replace(&self, range: Range<usize>, text: &str) -> bool {
        if self.state.borrow().refuse {
            return false;
        }
        let start = range.start;
        let removed = self.splice(range.clone(), text);
        self.state
            .borrow_mut()
            .replaces
            .push((range, text.to_owned()));
        self.tell(Note::Text {
            start,
            removed,
            inserted: text.to_owned(),
        });
        true
    }

    fn is_read_only(&self) -> bool {
        self.state.borrow().read_only
    }

    fn view_rect(&self) -> Option<Rect> {
        self.state.borrow().view
    }

    fn range_rect(&self, range: Range<usize>) -> Option<Rect> {
        let state = self.state.borrow();
        state.view?;
        let (first_line, first_col) = Self::line_col(&state.text, range.start);
        let (last_line, last_col) = Self::line_col(&state.text, range.end);
        Some(if first_line == last_line {
            Rect::new(
                Self::x(first_col),
                Self::y(first_line),
                (last_col - first_col) as f32 * CELL_W,
                CELL_H,
            )
        } else {
            Rect::from_ltrb(
                ORIGIN.0,
                Self::y(first_line),
                300.0,
                Self::y(last_line) + CELL_H,
            )
        })
    }

    fn composition_rect(&self, range: Range<usize>) -> Option<Rect> {
        let state = self.state.borrow();
        let overlay = state.overlay.as_ref()?;
        let (line, col) = Self::line_col(&state.text, state.selection.head);
        let first = overlay[..range.start].chars().count();
        let count = overlay[range.start..range.end].chars().count();
        Some(Rect::new(
            Self::x(col + first),
            Self::y(line),
            count as f32 * CELL_W,
            CELL_H,
        ))
    }

    fn offset_at_point(&self, point: Point) -> Option<usize> {
        let state = self.state.borrow();
        if !state.view?.contains(point) {
            return None;
        }
        let line = (((point.y - ORIGIN.1) / CELL_H).floor().max(0.0)) as usize;
        let col = (((point.x - ORIGIN.0) / CELL_W).round().max(0.0)) as usize;
        let mut offset = 0usize;
        for (index, text) in state.text.split('\n').enumerate() {
            if index == line.min(state.text.matches('\n').count()) {
                let chars = text.chars().count();
                let take = col.min(chars);
                return Some(offset + text.chars().take(take).map(char::len_utf8).sum::<usize>());
            }
            offset += text.len() + 1;
        }
        None
    }

    fn ime(&self, event: ImeEvent) {
        self.state.borrow_mut().events.push(event.clone());
        match event {
            ImeEvent::Start => {
                let selection = self.selection_now();
                if selection.start() != selection.end() {
                    let start = selection.start();
                    let removed = self.splice(selection.start()..selection.end(), "");
                    self.state.borrow_mut().selection = Selection::caret(start);
                    self.tell(Note::Text {
                        start,
                        removed,
                        inserted: String::new(),
                    });
                }
                self.state.borrow_mut().overlay = Some(String::new());
            }
            ImeEvent::Update { text, .. } => self.state.borrow_mut().overlay = Some(text),
            ImeEvent::Commit(text) => {
                self.state.borrow_mut().overlay = None;
                let selection = self.selection_now();
                self.edit(selection.start()..selection.end(), &text);
            }
            ImeEvent::End => self.state.borrow_mut().overlay = None,
        }
    }
}

// ---- scripted TSF sink -------------------------------------------------------------------------

type Log = Rc<RefCell<Vec<String>>>;
type Script = Rc<RefCell<Option<Box<dyn FnMut(u32)>>>>;

/// TSF's side of the store protocol: records every notification and runs a script while the
/// document is locked.
#[implement(ITextStoreACPSink)]
struct TestSink {
    events: Log,
    script: Script,
}

#[allow(non_snake_case)]
impl ITextStoreACPSink_Impl for TestSink_Impl {
    fn OnTextChange(
        &self,
        _dwflags: TEXT_STORE_TEXT_CHANGE_FLAGS,
        pchange: *const TS_TEXTCHANGE,
    ) -> windows::core::Result<()> {
        // SAFETY: the store passes a valid change for the duration of the call.
        let change = unsafe { *pchange };
        self.events.borrow_mut().push(format!(
            "text {}:{}->{}",
            change.acpStart, change.acpOldEnd, change.acpNewEnd
        ));
        Ok(())
    }

    fn OnSelectionChange(&self) -> windows::core::Result<()> {
        self.events.borrow_mut().push("sel".to_owned());
        Ok(())
    }

    fn OnLayoutChange(&self, lcode: TsLayoutCode, _vcview: u32) -> windows::core::Result<()> {
        self.events.borrow_mut().push(format!("layout {}", lcode.0));
        Ok(())
    }

    fn OnStatusChange(&self, dwflags: u32) -> windows::core::Result<()> {
        self.events.borrow_mut().push(format!("status {dwflags}"));
        Ok(())
    }

    fn OnAttrsChange(
        &self,
        _acpstart: i32,
        _acpend: i32,
        _cattrs: u32,
        _paattrs: *const GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnLockGranted(&self, dwlockflags: TEXT_STORE_LOCK_FLAGS) -> windows::core::Result<()> {
        self.events
            .borrow_mut()
            .push(format!("lock {}", dwlockflags.0));
        // Take the script out while it runs: it may trigger nested requests.
        let script = self.script.borrow_mut().take();
        if let Some(mut script) = script {
            script(dwlockflags.0);
            let mut slot = self.script.borrow_mut();
            if slot.is_none() {
                *slot = Some(script);
            }
        }
        Ok(())
    }

    fn OnStartEditTransaction(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnEndEditTransaction(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Typed calls on an `ITextStoreACP2`.
#[derive(Clone)]
struct Proxy(ITextStoreACP2);

impl Proxy {
    fn lock(&self, flags: u32) -> windows::core::Result<HRESULT> {
        com!(self.0.RequestLock(flags))
    }

    fn end(&self) -> windows::core::Result<i32> {
        com!(self.0.GetEndACP())
    }

    /// `(text, next, runs)`.
    fn text(
        &self,
        start: i32,
        end: i32,
        capacity: usize,
    ) -> windows::core::Result<(String, i32, u32)> {
        let mut buffer = vec![0u16; capacity];
        let (mut got, mut runs_got, mut next) = (0u32, 0u32, 0i32);
        let mut runs = [TS_RUNINFO::default(); 1];
        com!(self.0.GetText(
            start,
            end,
            &mut buffer,
            &mut got,
            &mut runs,
            &mut runs_got,
            &mut next
        ))?;
        Ok((
            String::from_utf16_lossy(&buffer[..got as usize]),
            next,
            runs_got,
        ))
    }

    /// `(start, end, backward)`.
    fn selection(&self) -> windows::core::Result<(i32, i32, bool)> {
        let mut selection = [TS_SELECTION_ACP::default(); 1];
        let mut fetched = 0u32;
        com!(
            self.0
                .GetSelection(TS_DEFAULT_SELECTION, &mut selection, &mut fetched)
        )?;
        assert_eq!(fetched, 1);
        Ok((
            selection[0].acpStart,
            selection[0].acpEnd,
            selection[0].style.ase == TS_AE_START,
        ))
    }

    fn set_selection(&self, start: i32, end: i32, backward: bool) -> windows::core::Result<()> {
        let selection = TS_SELECTION_ACP {
            acpStart: start,
            acpEnd: end,
            style: TS_SELECTIONSTYLE {
                ase: if backward { TS_AE_START } else { TS_AE_END },
                fInterimChar: false.into(),
            },
        };
        com!(self.0.SetSelection(&[selection]))
    }

    fn set_text(&self, start: i32, end: i32, text: &str) -> windows::core::Result<TS_TEXTCHANGE> {
        com!(self.0.SetText(0, start, end, &units(text)))
    }

    /// `(start, end, change)`.
    fn insert(&self, flags: u32, text: &str) -> windows::core::Result<(i32, i32, TS_TEXTCHANGE)> {
        let (mut start, mut end) = (0i32, 0i32);
        let mut change = TS_TEXTCHANGE::default();
        com!(
            self.0
                .InsertTextAtSelection(flags, &units(text), &mut start, &mut end, &mut change)
        )?;
        Ok((start, end, change))
    }

    fn ext(&self, start: i32, end: i32) -> windows::core::Result<(RECT, bool)> {
        let mut rect = RECT::default();
        let mut clipped = BOOL(0);
        com!(self.0.GetTextExt(0, start, end, &mut rect, &mut clipped))?;
        Ok((rect, clipped.as_bool()))
    }
}

/// A store over a test host with a scripted sink advised, on a hidden window.
struct Direct {
    proxy: Proxy,
    store: Rc<TextStore>,
    sink: ITextStoreACPSink,
    host: Rc<TestHost>,
    events: Log,
    script: Script,
    log: Log,
    window: HiddenWindow,
    _com: ComGuard,
}

impl Direct {
    fn new(text: &str) -> Self {
        let com = ComGuard::apartment_threaded().expect("COM initializes");
        let window = HiddenWindow::new();
        let host = TestHost::new(text);
        let store = Rc::new(TextStore::new(window.0, host.clone()));
        let weak = Rc::downgrade(&store);
        host.link(move |note| {
            if let Some(store) = weak.upgrade() {
                match note {
                    Note::Text {
                        start,
                        removed,
                        inserted,
                    } => store.text_changed(start, &removed, &inserted),
                    Note::Selection => store.selection_changed(),
                }
            }
        });
        let events: Log = Rc::default();
        let script: Script = Rc::default();
        let sink: ITextStoreACPSink = TestSink {
            events: events.clone(),
            script: script.clone(),
        }
        .into();
        let unknown: IUnknown = sink.cast().expect("the sink is an IUnknown");
        com!(store.interface().AdviseSink(
            &ITextStoreACPSink::IID,
            &unknown,
            TS_AS_TEXT_CHANGE | TS_AS_SEL_CHANGE | TS_AS_LAYOUT_CHANGE | TS_AS_STATUS_CHANGE
        ))
        .expect("the sink is advised");
        Self {
            proxy: Proxy(store.interface().clone()),
            store,
            sink,
            host,
            events,
            script,
            log: Rc::default(),
            window,
            _com: com,
        }
    }

    /// Requests a lock and runs `script` inside it. Returns the session HRESULT.
    fn locked(&self, flags: u32, script: impl FnMut(u32) + 'static) -> HRESULT {
        *self.script.borrow_mut() = Some(Box::new(script));
        let hr = self.proxy.lock(flags).expect("RequestLock succeeds");
        *self.script.borrow_mut() = None;
        hr
    }

    /// [`Direct::locked`] for scripts whose own results are logged: the sink's `OnLockGranted`
    /// returns `S_OK` whatever the script's requests got, so the session result must be `S_OK`.
    fn locked_ok(&self, flags: u32, script: impl FnMut(u32) + 'static) {
        assert_eq!(
            self.locked(flags, script),
            HRESULT(0),
            "a granted lock reports the sink's S_OK"
        );
    }

    fn sink_events(&self) -> Vec<String> {
        self.events.borrow().clone()
    }

    fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut *self.events.borrow_mut())
    }

    fn logged(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    fn composition_sink(&self) -> ITfContextOwnerCompositionSink {
        self.proxy
            .0
            .cast()
            .expect("the store is a composition sink")
    }
}

impl Drop for Direct {
    fn drop(&mut self) {
        *self.script.borrow_mut() = None;
        if let Ok(unknown) = self.sink.cast::<IUnknown>() {
            let _ = com!(self.proxy.0.UnadviseSink(&unknown));
        }
    }
}

const RW_SYNC: u32 = TS_LF_SYNC | TS_LF_READWRITE.0;
const READ_SYNC: u32 = TS_LF_SYNC | TS_LF_READ.0;

// ---- direct protocol tests ---------------------------------------------------------------------

#[test]
fn direct_requests_without_a_lock_are_refused() {
    let d = Direct::new("abc");
    let p = &d.proxy;
    assert_eq!(code(p.end()), TS_E_NOLOCK);
    assert_eq!(code(p.text(0, -1, 8)), TS_E_NOLOCK);
    assert_eq!(code(p.selection()), TS_E_NOLOCK);
    assert_eq!(code(p.set_text(0, 0, "x")), TS_E_NOLOCK);
    assert_eq!(code(p.set_selection(0, 0, false)), TS_E_NOLOCK);
    assert_eq!(code(p.insert(0, "x")), TS_E_NOLOCK);
    assert_eq!(code(p.ext(0, 0)), TS_E_NOLOCK);
    assert_eq!(d.host.doc(), "abc");
    // No lock needed for these.
    let status = com!(p.0.GetStatus()).expect("GetStatus");
    assert_eq!(status.dwDynamicFlags, 0);
    assert_eq!(status.dwStaticFlags, TS_SS_NOHIDDENTEXT);
    assert_eq!(com!(p.0.GetActiveView()).expect("GetActiveView"), 0);
    assert!(com!(p.0.GetScreenExt(0)).is_ok());
}

#[test]
fn direct_a_sync_read_lock_reads_utf16_text_runs_and_selection() {
    let d = Direct::new("a😀b");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    let hr = d.locked(READ_SYNC, move |flags| {
        let mut log = log.borrow_mut();
        log.push(format!("flags {flags}"));
        log.push(format!("end {:?}", p.end().ok()));
        log.push(format!("all {:?}", p.text(0, -1, 100).ok()));
        log.push(format!("cut {:?}", p.text(0, -1, 2).ok()));
        log.push(format!("pair {:?}", p.text(1, 3, 100).ok()));
        log.push(format!("selection {:?}", p.selection().ok()));
        log.push(format!(
            "reversed {}",
            p.text(3, 1, 8).err().map_or(0, |e| e.code().0)
        ));
        log.push(format!(
            "past {}",
            p.text(0, 9, 8).err().map_or(0, |e| e.code().0)
        ));
        log.push(format!(
            "negative {}",
            p.text(0, -5, 8).err().map_or(0, |e| e.code().0)
        ));
        log.push(format!("index {}", {
            let mut one = [TS_SELECTION_ACP::default(); 1];
            let mut fetched = 7u32;
            let result = com!(p.0.GetSelection(3, &mut one, &mut fetched));
            result.err().map_or(0, |e| e.code().0)
        }));
        log.push(format!("zero {}", {
            let mut fetched = 7u32;
            let result = com!(p.0.GetSelection(0, &mut [], &mut fetched));
            format!("{} {fetched}", result.is_ok())
        }));
    });
    assert_eq!(hr, HRESULT(0));
    assert_eq!(
        d.logged(),
        vec![
            "flags 2".to_owned(),
            "end Some(4)".to_owned(),
            "all Some((\"a😀b\", 4, 1))".to_owned(),
            // A buffer of 2 would cut the surrogate pair: only "a" is returned.
            "cut Some((\"a\", 1, 1))".to_owned(),
            "pair Some((\"😀\", 3, 1))".to_owned(),
            "selection Some((4, 4, false))".to_owned(),
            format!("reversed {}", TS_E_INVALIDPOS.0),
            format!("past {}", TS_E_INVALIDPOS.0),
            format!("negative {}", TS_E_INVALIDPOS.0),
            format!("index {}", TS_E_NOSELECTION.0),
            "zero true 0".to_owned(),
        ]
    );
    assert_eq!(d.sink_events(), vec!["lock 2".to_owned()]);
}

#[test]
fn direct_invalid_and_unadvised_lock_requests_fail() {
    let d = Direct::new("abc");
    assert_eq!(code(d.proxy.lock(4)), E_INVALIDARG);
    assert_eq!(code(d.proxy.lock(TS_LF_SYNC)), E_INVALIDARG);
    let unknown: IUnknown = d.sink.cast().expect("IUnknown");
    com!(d.proxy.0.UnadviseSink(&unknown)).expect("unadvise");
    assert_eq!(code(d.proxy.lock(READ_SYNC)), E_UNEXPECTED);
}

#[test]
fn direct_nested_requests_follow_the_lock_protocol() {
    let d = Direct::new("x");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    let calls = Rc::new(Cell::new(0));
    let hr = d.locked(READ_SYNC, move |_| {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            let mut log = log.borrow_mut();
            log.push(format!("sync {:x}", p.lock(READ_SYNC).expect("call").0));
            log.push(format!("async rw {:x}", p.lock(6).expect("call").0));
            // A weaker request does not downgrade the queued one.
            log.push(format!("async r {:x}", p.lock(2).expect("call").0));
        }
    });
    assert_eq!(hr, HRESULT(0));
    assert_eq!(
        d.logged(),
        vec![
            format!("sync {:x}", TS_E_SYNCHRONOUS.0),
            format!("async rw {:x}", TS_S_ASYNC.0),
            format!("async r {:x}", TS_S_ASYNC.0),
        ]
    );
    // The queued request ran after the release, as a read/write lock.
    assert_eq!(
        d.sink_events(),
        vec!["lock 2".to_owned(), "lock 6".to_owned()]
    );
}

#[test]
fn direct_writes_need_a_read_write_lock() {
    let d = Direct::new("abc");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!("set_text {:x}", code(p.set_text(0, 1, "x")).0));
        log.push(format!(
            "set_selection {:x}",
            code(p.set_selection(0, 1, false)).0
        ));
        log.push(format!("insert {:x}", code(p.insert(0, "x")).0));
        // A query-only insert reads: it reports the range without editing.
        log.push(format!(
            "query {:?}",
            p.insert(TS_IAS_QUERYONLY, "xy")
                .map(|(s, e, c)| (s, e, c.acpNewEnd))
        ));
    });
    assert_eq!(
        d.logged(),
        vec![
            format!("set_text {:x}", TS_E_NOLOCK.0),
            format!("set_selection {:x}", TS_E_NOLOCK.0),
            format!("insert {:x}", TS_E_NOLOCK.0),
            "query Ok((3, 5, 0))".to_owned(),
        ]
    );
    assert_eq!(d.host.doc(), "abc");
}

#[test]
fn direct_set_text_edits_the_host_and_is_not_echoed_back() {
    let d = Direct::new("hello");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let result = p.set_text(0, 5, "abc");
        log.borrow_mut().push(format!(
            "{:?}",
            result.map(|c| (c.acpStart, c.acpOldEnd, c.acpNewEnd))
        ));
        log.borrow_mut().push(format!("end {:?}", p.end().ok()));
        // Past-the-end and reversed ranges are invalid; a lone surrogate cannot be stored.
        log.borrow_mut()
            .push(format!("past {:x}", code(p.set_text(0, 9, "x")).0));
        log.borrow_mut().push(format!(
            "lone {:x}",
            code(com!(p.0.SetText(0, 0, 0, &[0xD800]))).0
        ));
    });
    assert_eq!(
        d.logged(),
        vec![
            "Ok((0, 5, 3))".to_owned(),
            "end Some(3)".to_owned(),
            format!("past {:x}", TS_E_INVALIDPOS.0),
            format!("lone {:x}", E_INVALIDARG.0),
        ]
    );
    assert_eq!(d.host.doc(), "abc");
    assert_eq!(
        d.host.state.borrow().replaces,
        vec![(0..5, "abc".to_owned())]
    );
    // TSF requested the edit: no OnTextChange / OnSelectionChange echo, only the layout change.
    assert_eq!(
        d.sink_events(),
        vec!["lock 6".to_owned(), "layout 1".to_owned()]
    );
}

#[test]
fn direct_a_refusing_or_read_only_host_fails_edits_with_ts_e_readonly() {
    let d = Direct::new("abc");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.host.set_refuse(true);
    d.locked_ok(RW_SYNC, {
        let (p, log) = (p.clone(), log.clone());
        move |_| {
            log.borrow_mut()
                .push(format!("refused {:x}", code(p.set_text(0, 1, "x")).0));
        }
    });
    d.host.set_refuse(false);
    d.host.set_read_only(true);
    d.store.status_changed();
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!("set_text {:x}", code(p.set_text(0, 1, "x")).0));
        log.push(format!("insert {:x}", code(p.insert(0, "x")).0));
    });
    assert_eq!(
        d.logged(),
        vec![
            format!("refused {:x}", TS_E_READONLY.0),
            format!("set_text {:x}", TS_E_READONLY.0),
            format!("insert {:x}", TS_E_READONLY.0),
        ]
    );
    assert_eq!(d.host.doc(), "abc");
    let status = com!(d.proxy.0.GetStatus()).expect("GetStatus");
    assert_eq!(status.dwDynamicFlags, TS_SD_READONLY);
    assert!(
        d.sink_events()
            .contains(&format!("status {TS_SD_READONLY}")),
        "{:?}",
        d.sink_events()
    );
}

#[test]
fn direct_insert_at_selection_replaces_the_selection_and_moves_the_caret() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::new(1, 3));
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!(
            "query {:?}",
            p.insert(TS_IAS_QUERYONLY, "😀")
                .map(|(s, e, c)| (s, e, c.acpNewEnd))
        ));
        log.push(format!(
            "insert {:?}",
            p.insert(0, "😀")
                .map(|(s, e, c)| (s, e, (c.acpStart, c.acpOldEnd, c.acpNewEnd)))
        ));
        log.push(format!("selection {:?}", p.selection().ok()));
    });
    assert_eq!(
        d.logged(),
        vec![
            "query Ok((1, 3, 0))".to_owned(),
            "insert Ok((1, 3, (1, 3, 3)))".to_owned(),
            "selection Some((3, 3, false))".to_owned(),
        ]
    );
    assert_eq!(d.host.doc(), "a😀d");
    assert_eq!(d.host.selection_now(), Selection::caret(5));
}

#[test]
fn direct_selection_round_trips_with_direction_and_validates() {
    let d = Direct::new("a😀bc");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!("set {:?}", p.set_selection(1, 4, true).is_ok()));
        log.push(format!("get {:?}", p.selection().ok()));
        // A caret between the two units of a pair moves to the start of the pair.
        log.push(format!("caret {:?}", p.set_selection(2, 2, false).is_ok()));
        log.push(format!("get {:?}", p.selection().ok()));
        log.push(format!(
            "reversed {:x}",
            code(p.set_selection(3, 1, false)).0
        ));
        log.push(format!("past {:x}", code(p.set_selection(0, 99, false)).0));
        let two = [TS_SELECTION_ACP::default(); 2];
        log.push(format!("two {:x}", code(com!(p.0.SetSelection(&two))).0));
    });
    assert_eq!(
        d.logged(),
        vec![
            "set true".to_owned(),
            "get Some((1, 4, true))".to_owned(),
            "caret true".to_owned(),
            "get Some((1, 1, false))".to_owned(),
            format!("reversed {:x}", TS_E_INVALIDPOS.0),
            format!("past {:x}", TS_E_INVALIDPOS.0),
            format!("two {:x}", E_INVALIDARG.0),
        ]
    );
    // The last valid selection is the caret at ACP 1 = byte 1.
    assert_eq!(d.host.selection_now(), Selection::caret(1));
}

#[test]
fn direct_backward_selection_maps_to_a_reversed_host_selection() {
    let d = Direct::new("a😀bc");
    let p = d.proxy.clone();
    d.locked_ok(RW_SYNC, move |_| {
        let _ = p.set_selection(1, 4, true);
    });
    // ACP 1..4 is bytes 1..6 ("😀b"); backward: the caret (head) is at the start.
    assert_eq!(d.host.selection_now(), Selection::new(6, 1));
}

#[test]
fn direct_query_insert_snaps_to_scalar_boundaries() {
    let d = Direct::new("a😀b");
    let query = |start: i32, end: i32| {
        let (mut a, mut b) = (0i32, 0i32);
        com!(d.proxy.0.QueryInsert(start, end, 1, &mut a, &mut b)).expect("QueryInsert");
        (a, b)
    };
    assert_eq!(query(2, 2), (1, 1));
    assert_eq!(query(2, 3), (1, 3));
    assert_eq!(query(0, 2), (0, 3));
    assert_eq!(query(0, 99), (0, 4));
    assert_eq!(query(-4, 1), (0, 1));
}

#[test]
fn direct_host_edits_are_notified_in_protocol_order() {
    let d = Direct::new("abc");
    d.host.edit(1..2, "😀");
    assert_eq!(
        d.take_events(),
        vec![
            "text 1:2->3".to_owned(),
            "sel".to_owned(),
            "layout 1".to_owned()
        ]
    );
    d.host.move_caret(Selection::new(0, 2));
    assert_eq!(d.take_events(), vec!["sel".to_owned()]);
    d.store.layout_changed();
    assert_eq!(d.take_events(), vec!["layout 1".to_owned()]);
    d.store.layout_created();
    d.store.layout_destroyed();
    assert_eq!(
        d.take_events(),
        vec!["layout 0".to_owned(), "layout 2".to_owned()]
    );
}

#[test]
fn direct_notifications_are_deferred_while_locked_and_merged_before_the_queued_lock() {
    let d = Direct::new("abcdef");
    let (p, log, host) = (d.proxy.clone(), d.log.clone(), d.host.clone());
    let calls = Rc::new(Cell::new(0));
    d.locked_ok(RW_SYNC, move |_| {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            host.edit(0..0, "X");
            host.edit(4..5, "YY");
            log.borrow_mut()
                .push(format!("async {:x}", p.lock(2).expect("call").0));
        }
    });
    assert_eq!(d.host.doc(), "XabcYYef");
    assert_eq!(d.logged(), vec![format!("async {:x}", TS_S_ASYNC.0)]);
    // Nothing during the lock; one merged change covering "abcd" -> "XabcYY" after it, before
    // the queued read lock.
    assert_eq!(
        d.sink_events(),
        vec![
            "lock 6".to_owned(),
            "text 0:4->6".to_owned(),
            "sel".to_owned(),
            "layout 1".to_owned(),
            "lock 2".to_owned(),
        ]
    );
}

#[test]
fn direct_advise_and_unadvise_follow_the_connection_rules() {
    let d = Direct::new("abc");
    let other: ITextStoreACPSink = TestSink {
        events: Rc::default(),
        script: Rc::default(),
    }
    .into();
    let other_unknown: IUnknown = other.cast().expect("IUnknown");
    let own_unknown: IUnknown = d.sink.cast().expect("IUnknown");
    let advise =
        |iid: &GUID, sink: &IUnknown, mask: u32| com!(d.proxy.0.AdviseSink(iid, sink, mask));
    // A second, different sink, and a foreign interface id, are refused.
    assert_eq!(
        code(advise(&ITextStoreACPSink::IID, &other_unknown, 0)),
        CONNECT_E_ADVISELIMIT
    );
    assert_eq!(
        code(advise(&IUnknown::IID, &own_unknown, 0)),
        CONNECT_E_CANNOTCONNECT
    );
    assert_eq!(
        code(com!(d.proxy.0.UnadviseSink(&other_unknown))),
        CONNECT_E_NOCONNECTION
    );
    assert!(
        com!(
            d.proxy
                .0
                .AdviseSink(&ITextStoreACPSink::IID, None::<&IUnknown>, 0)
        )
        .is_err()
    );
    // Advising the same sink again changes the mask: with an empty mask nothing is sent.
    advise(&ITextStoreACPSink::IID, &own_unknown, 0).expect("re-advise");
    d.host.edit(0..0, "Z");
    assert!(d.sink_events().is_empty(), "{:?}", d.sink_events());
    advise(&ITextStoreACPSink::IID, &own_unknown, TS_AS_SEL_CHANGE).expect("re-advise");
    d.host.edit(0..0, "Z");
    assert_eq!(d.take_events(), vec!["sel".to_owned()]);
    // Unadvising ends the connection.
    com!(d.proxy.0.UnadviseSink(&own_unknown)).expect("unadvise");
    d.host.edit(0..0, "Z");
    assert!(d.sink_events().is_empty());
    assert_eq!(
        code(com!(d.proxy.0.UnadviseSink(&own_unknown))),
        CONNECT_E_NOCONNECTION
    );
}

#[test]
fn direct_layout_queries_answer_in_screen_coordinates() {
    let d = Direct::new("ab\ncd😀");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        let mut log = log.borrow_mut();
        // caret between a and b, "ab", "cd", across the line break, the emoji
        for (start, end) in [(1, 1), (0, 2), (3, 5), (1, 4), (5, 7)] {
            log.push(format!(
                "{start}..{end} {:?}",
                p.ext(start, end)
                    .map(|(r, c)| (r.left, r.top, r.right, r.bottom, c))
                    .ok()
            ));
        }
        log.push(format!("past {:x}", code(p.ext(0, 99)).0));
        log.push(format!("reversed {:x}", code(p.ext(3, 1)).0));
        let mut rect = RECT::default();
        let mut clipped = BOOL(0);
        log.push(format!(
            "view {:x}",
            code(com!(p.0.GetTextExt(1, 0, 1, &mut rect, &mut clipped))).0
        ));
    });
    let w = &d.window;
    let expect = |rect: Rect, clipped: bool| {
        let r = w.screen_rect(rect);
        format!(
            "Some(({}, {}, {}, {}, {clipped}))",
            r.left, r.top, r.right, r.bottom
        )
    };
    let logged = d.logged();
    let line = |index: usize, start: i32, end: i32, rect: Rect| {
        assert_eq!(
            logged[index],
            format!("{start}..{end} {}", expect(rect, false))
        );
    };
    line(0, 1, 1, Rect::new(14.0, 6.0, 0.0, 20.0));
    line(1, 0, 2, Rect::new(4.0, 6.0, 20.0, 20.0));
    line(2, 3, 5, Rect::new(4.0, 26.0, 20.0, 20.0));
    line(3, 1, 4, Rect::from_ltrb(4.0, 6.0, 300.0, 46.0));
    // The emoji is one cell wide although it is two ACPs.
    line(4, 5, 7, Rect::new(24.0, 26.0, 10.0, 20.0));
    assert_eq!(logged[5], format!("past {:x}", TS_E_INVALIDPOS.0));
    assert_eq!(logged[6], format!("reversed {:x}", TS_E_INVALIDPOS.0));
    assert_eq!(logged[7], format!("view {:x}", E_INVALIDARG.0));
    // The screen extent is the view rectangle.
    let screen = com!(d.proxy.0.GetScreenExt(0)).expect("GetScreenExt");
    let expected = d.window.screen_rect(Rect::new(0.0, 0.0, 300.0, 100.0));
    assert_eq!(
        (screen.left, screen.top, screen.right, screen.bottom),
        (expected.left, expected.top, expected.right, expected.bottom)
    );
    assert_eq!(code(com!(d.proxy.0.GetScreenExt(1))), E_INVALIDARG);
}

#[test]
fn direct_text_clipped_by_the_view_and_missing_layout_are_reported() {
    let d = Direct::new("ab\ncd");
    d.host.set_view(Some(Rect::new(0.0, 0.0, 300.0, 30.0)));
    let (p, log) = (d.proxy.clone(), d.log.clone());
    let host = d.host.clone();
    d.locked_ok(READ_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!(
            "second line {:?}",
            p.ext(3, 5).map(|(_, clipped)| clipped).ok()
        ));
        log.push(format!(
            "first line {:?}",
            p.ext(0, 2).map(|(_, clipped)| clipped).ok()
        ));
        host.set_view(None);
        log.push(format!("no layout {:x}", code(p.ext(0, 2)).0));
        log.push(format!("no screen {:x}", code(com!(p.0.GetScreenExt(0))).0));
    });
    assert_eq!(
        d.logged(),
        vec![
            "second line Some(true)".to_owned(),
            "first line Some(false)".to_owned(),
            format!("no layout {:x}", TS_E_NOLAYOUT.0),
            format!("no screen {:x}", TS_E_NOLAYOUT.0),
        ]
    );
}

#[test]
fn direct_points_map_to_acps_through_utf16() {
    let d = Direct::new("ab\ncd😀");
    let at = |x: f32, y: f32| {
        let point = d.window.screen_point(x, y);
        com!(d.proxy.0.GetACPFromPoint(0, &point, 0)).map_err(|e| e.code())
    };
    // line 0, column 1 -> ACP 1; line 1, column 2 -> ACP 5; after the emoji -> ACP 7
    assert_eq!(at(14.0, 10.0), Ok(1));
    assert_eq!(at(24.0, 30.0), Ok(5));
    assert_eq!(at(34.0, 30.0), Ok(7));
    // Outside the view: the host has no offset there.
    assert_eq!(at(-50.0, 10.0), Err(TS_E_INVALIDPOINT));
    let origin = d.window.screen_point(0.0, 0.0);
    assert_eq!(
        code(com!(d.proxy.0.GetACPFromPoint(1, &origin, 0))),
        E_INVALIDARG
    );
}

#[test]
fn direct_input_scope_is_served_once_per_request_as_plain_text() {
    let d = Direct::new("abc");
    let retrieve = || {
        let mut values = [TS_ATTRVAL::default()];
        let mut fetched = 0u32;
        com!(d.proxy.0.RetrieveRequestedAttrs(&mut values, &mut fetched)).expect("Retrieve");
        (values, fetched)
    };
    let scopes_of = |values: &[TS_ATTRVAL; 1]| {
        assert_eq!(values[0].idAttr, GUID_PROP_INPUTSCOPE);
        let unknown = IUnknown::try_from(&values[0].varValue).expect("VT_UNKNOWN");
        let scope: ITfInputScope = unknown.cast().expect("ITfInputScope");
        let mut array: *mut InputScope = std::ptr::null_mut();
        let mut count = 0u32;
        com!(scope.GetInputScopes(&mut array, &mut count)).expect("GetInputScopes");
        // SAFETY: the object returned `count` scopes in CoTaskMem memory, freed right after.
        let read: Vec<InputScope> = (0..count as usize)
            .map(|i| unsafe { *array.add(i) })
            .collect();
        // SAFETY: `array` was allocated with CoTaskMemAlloc by GetInputScopes.
        unsafe { CoTaskMemFree(Some(array as *const c_void)) };
        let mut phrases: *mut BSTR = std::ptr::null_mut();
        let mut phrase_count = 0u32;
        assert_eq!(
            code(com!(scope.GetPhrase(&mut phrases, &mut phrase_count))),
            E_NOTIMPL
        );
        assert_eq!(code(com!(scope.GetRegularExpression())), E_NOTIMPL);
        assert_eq!(code(com!(scope.GetSRGS())), E_NOTIMPL);
        assert_eq!(code(com!(scope.GetXML())), E_NOTIMPL);
        read
    };
    // Nothing requested: nothing served.
    assert_eq!(retrieve().1, 0);
    // Requested by id.
    com!(
        d.proxy
            .0
            .RequestSupportedAttrs(TS_ATTR_FIND_WANT_VALUE, &[GUID_PROP_INPUTSCOPE])
    )
    .expect("request");
    let (values, fetched) = retrieve();
    assert_eq!(fetched, 1);
    assert_eq!(scopes_of(&values), vec![IS_DEFAULT]);
    // Served once.
    assert_eq!(retrieve().1, 0);
    // Another attribute does not trigger it; an empty filter means every attribute.
    com!(d.proxy.0.RequestSupportedAttrs(0, &[GUID_PROP_ATTRIBUTE])).expect("request");
    assert_eq!(retrieve().1, 0);
    com!(d.proxy.0.RequestSupportedAttrs(0, &[])).expect("request");
    assert_eq!(retrieve().1, 1);
    // At a position.
    com!(
        d.proxy
            .0
            .RequestAttrsAtPosition(0, &[GUID_PROP_INPUTSCOPE], 0)
    )
    .expect("request");
    assert_eq!(retrieve().1, 1);
    // The input scope never transitions inside the text.
    com!(
        d.proxy
            .0
            .RequestAttrsTransitioningAtPosition(0, &[GUID_PROP_INPUTSCOPE], 0)
    )
    .expect("request");
    assert_eq!(retrieve().1, 0);
    let (mut next, mut found, mut offset) = (0i32, BOOL(1), 9i32);
    com!(d.proxy.0.FindNextAttrTransition(
        0,
        3,
        &[GUID_PROP_INPUTSCOPE],
        0,
        &mut next,
        &mut found,
        &mut offset
    ))
    .expect("find");
    assert_eq!((next, found.as_bool(), offset), (3, false, 0));
    // No buffer slots: nothing fetched.
    let mut fetched = 5u32;
    com!(d.proxy.0.RetrieveRequestedAttrs(&mut [], &mut fetched)).expect("Retrieve");
    assert_eq!(fetched, 0);
}

#[test]
fn direct_unsupported_features_report_not_implemented() {
    let d = Direct::new("abc");
    assert_eq!(code(com!(d.proxy.0.GetFormattedText(0, 1))), E_NOTIMPL);
    assert_eq!(
        code(com!(d.proxy.0.GetEmbedded(
            0,
            &GUID::zeroed(),
            &GUID::zeroed()
        ))),
        E_NOTIMPL
    );
    assert!(
        !com!(
            d.proxy
                .0
                .QueryInsertEmbedded(std::ptr::null(), std::ptr::null())
        )
        .expect("QueryInsertEmbedded")
        .as_bool()
    );
    assert_eq!(
        code(com!(d.proxy.0.InsertEmbedded(
            0,
            0,
            0,
            None::<&windows::Win32::System::Com::IDataObject>
        ))),
        E_NOTIMPL
    );
    let (mut a, mut b, mut c) = (0, 0, TS_TEXTCHANGE::default());
    assert_eq!(
        code(com!(d.proxy.0.InsertEmbeddedAtSelection(
            0,
            None::<&windows::Win32::System::Com::IDataObject>,
            &mut a,
            &mut b,
            &mut c
        ))),
        E_NOTIMPL
    );
    // Null out pointers are rejected, not dereferenced.
    assert_eq!(
        code(com!(d.proxy.0.GetTextExt(
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut()
        ))),
        E_POINTER
    );
}

// ---- direct composition tests ------------------------------------------------------------------

fn update(text: &str, caret: usize, attrs: &[(Range<usize>, ImeAttrStyle)]) -> ImeEvent {
    ImeEvent::Update {
        text: text.to_owned(),
        caret,
        attrs: attrs
            .iter()
            .map(|(range, style)| ImeAttr {
                range: range.clone(),
                style: *style,
            })
            .collect(),
    }
}

#[test]
fn direct_composition_shows_in_tsf_text_and_commits_to_the_host() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::caret(2));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    assert!(d.store.is_composing());
    assert_eq!(d.host.events_now(), vec![ImeEvent::Start]);
    assert_eq!(d.host.overlay_now(), Some(String::new()));

    let (p, log) = (d.proxy.clone(), d.log.clone());
    let comp = sink.clone();
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!(
            "set {:?}",
            p.set_text(2, 2, "かな")
                .map(|c| (c.acpStart, c.acpOldEnd, c.acpNewEnd))
        ));
        log.push(format!("caret {:?}", p.set_selection(4, 4, false).is_ok()));
        // TSF reads its own text, the composition spliced into the document.
        log.push(format!("text {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
        log.push(format!("end {:?}", p.end().ok()));
        log.push(format!(
            "update {:?}",
            com!(comp.OnUpdateComposition(None::<&ITfCompositionView>, None::<&ITfRange>)).is_ok()
        ));
    });
    assert_eq!(
        d.logged(),
        vec![
            "set Ok((2, 2, 4))".to_owned(),
            "caret true".to_owned(),
            "text Some(\"abかなcd\")".to_owned(),
            "end Some(6)".to_owned(),
            "update true".to_owned(),
        ]
    );
    // The host document is untouched; the host shows the string as an overlay, told once the
    // lock was released.
    assert_eq!(d.host.doc(), "abcd");
    assert_eq!(
        d.host.events_now(),
        vec![
            ImeEvent::Start,
            update("かな", 6, &[(0..6, ImeAttrStyle::Input)]),
        ]
    );
    assert_eq!(d.host.overlay_now(), Some("かな".to_owned()));

    // Where the candidate window goes: the composition string in the overlay.
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        let rect = p
            .ext(2, 4)
            .map(|(r, _)| (r.right - r.left, r.bottom - r.top))
            .ok();
        log.borrow_mut().push(format!("ext {rect:?}"));
    });
    let expected = d.window.screen_rect(Rect::new(24.0, 6.0, 20.0, 20.0));
    assert_eq!(
        d.logged().last().cloned(),
        Some(format!(
            "ext Some(({}, {}))",
            expected.right - expected.left,
            expected.bottom - expected.top
        ))
    );

    // Ending the composition commits the string at the host selection.
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert!(!d.store.is_composing());
    assert_eq!(d.host.doc(), "abかなcd");
    assert_eq!(d.host.selection_now(), Selection::caret(8));
    assert_eq!(d.host.overlay_now(), None);
    let events = d.host.events_now();
    assert_eq!(
        events[events.len() - 2..],
        [ImeEvent::Commit("かな".to_owned()), ImeEvent::End]
    );
    // TSF sees the same text as before the commit.
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        log.borrow_mut().push(format!(
            "final {:?} {:?}",
            p.text(0, -1, 100).map(|t| t.0).ok(),
            p.selection().ok()
        ));
    });
    assert_eq!(
        d.logged().last().cloned(),
        Some("final Some(\"abかなcd\") Some((4, 4, false))".to_owned())
    );
}

#[test]
fn direct_cancelled_composition_ends_without_a_commit() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::caret(2));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert_eq!(d.host.events_now(), vec![ImeEvent::Start, ImeEvent::End]);
    assert_eq!(d.host.doc(), "abcd");
    assert_eq!(d.host.selection_now(), Selection::caret(2));
    // Ending when nothing composes does nothing.
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert_eq!(d.host.events_now().len(), 2);
}

#[test]
fn direct_composition_over_a_selection_replaces_it_and_shows_it_as_the_composition() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::new(1, 3));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    // The host removed the selected text and shows it as the composition string.
    assert_eq!(d.host.doc(), "ad");
    assert_eq!(
        d.host.events_now(),
        vec![
            ImeEvent::Start,
            update("bc", 2, &[(0..2, ImeAttrStyle::Input)])
        ]
    );
    assert_eq!(d.host.overlay_now(), Some("bc".to_owned()));

    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        // TSF still sees the original text, the selection covers the composition.
        log.push(format!("text {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
        log.push(format!("selection {:?}", p.selection().ok()));
        // A range across the composition boundary cannot be edited.
        log.push(format!("straddle {:x}", code(p.set_text(0, 2, "x")).0));
        log.push(format!("straddle ext {:?}", p.ext(0, 4).is_ok()));
        // Edits before the composition go to the host and keep it anchored.
        log.push(format!(
            "before {:?}",
            p.set_text(0, 1, "AA")
                .map(|c| (c.acpStart, c.acpOldEnd, c.acpNewEnd))
        ));
        log.push(format!("text {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
        // ... and edits after it too.
        log.push(format!(
            "after {:?}",
            p.set_text(4, 5, "DD")
                .map(|c| (c.acpStart, c.acpOldEnd, c.acpNewEnd))
        ));
        log.push(format!("text {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
        // Replace the composition text.
        log.push(format!(
            "replace {:?}",
            p.set_text(2, 4, "BC")
                .map(|c| (c.acpStart, c.acpOldEnd, c.acpNewEnd))
        ));
    });
    assert_eq!(
        d.logged(),
        vec![
            "text Some(\"abcd\")".to_owned(),
            "selection Some((1, 3, false))".to_owned(),
            format!("straddle {:x}", TS_E_INVALIDPOS.0),
            "straddle ext true".to_owned(),
            "before Ok((0, 1, 2))".to_owned(),
            "text Some(\"AAbcd\")".to_owned(),
            "after Ok((4, 5, 6))".to_owned(),
            "text Some(\"AAbcDD\")".to_owned(),
            "replace Ok((2, 4, 4))".to_owned(),
        ]
    );
    assert_eq!(d.host.doc(), "AADD");
    assert_eq!(d.host.overlay_now(), Some("BC".to_owned()));
    com!(
        d.composition_sink()
            .OnEndComposition(None::<&ITfCompositionView>)
    )
    .expect("end");
    assert_eq!(d.host.doc(), "AABCDD");
}

#[test]
fn direct_hosts_edits_and_selection_changes_while_composing_do_not_confuse_tsf() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::caret(2));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    d.take_events();
    // The user's caret move is not reported: TSF owns the selection while composing.
    d.host.move_caret(Selection::caret(1));
    assert!(d.sink_events().is_empty(), "{:?}", d.sink_events());
    // A host edit before the composition is reported in ACPs of the TSF text, and keeps the
    // composition anchored to the same host text.
    d.host.edit(0..0, "ZZ");
    assert_eq!(
        d.take_events(),
        vec![
            "text 0:0->2".to_owned(),
            "sel".to_owned(),
            "layout 1".to_owned()
        ]
    );
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!(
            "insert {:?}",
            p.insert(0, "x").map(|(s, e, _)| (s, e)).ok()
        ));
        log.push(format!("text {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
    });
    // The composition started at host offset 2 ("ab|cd"); the host edit moved it to 4, and TSF's
    // selection (the composition's insertion point) moved with it: the insertion lands in the
    // composition string, not in the host document.
    assert_eq!(
        d.logged(),
        vec![
            "insert Some((4, 5))".to_owned(),
            "text Some(\"ZZabxcd\")".to_owned()
        ]
    );
    assert_eq!(d.host.doc(), "ZZabcd");
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert_eq!(d.host.doc(), "ZZabxcd");
}

#[test]
fn direct_composition_is_rejected_when_read_only_or_already_composing() {
    let d = Direct::new("abc");
    let sink = d.composition_sink();
    d.host.set_read_only(true);
    assert!(
        !com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    assert!(!d.store.is_composing());
    assert!(d.host.events_now().is_empty());
    d.host.set_read_only(false);
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    assert!(
        !com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    // Updates without a composition are ignored.
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    com!(sink.OnUpdateComposition(None::<&ITfCompositionView>, None::<&ITfRange>)).expect("update");
}

#[test]
fn direct_unadvise_inside_a_lock_drops_the_queued_grant() {
    let d = Direct::new("abc");
    let (p, store_iface) = (d.proxy.clone(), d.proxy.0.clone());
    let unknown: IUnknown = d.sink.cast().expect("IUnknown");
    d.locked_ok(READ_SYNC, move |_| {
        let _ = p.lock(6);
        let _ = com!(store_iface.UnadviseSink(&unknown));
    });
    assert_eq!(d.sink_events(), vec!["lock 2".to_owned()]);
}

#[test]
fn direct_a_document_switch_resets_the_cache_and_tells_tsf_the_whole_text_changed() {
    let d = Direct::new("abc");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!("before {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
    });
    d.take_events();

    // The editor shows a longer document with a surrogate pair; no edit is reported.
    d.host.switch_to("héllo 😀");
    d.store.document_replaced();
    assert_eq!(
        d.take_events(),
        vec![
            "text 0:3->8".to_owned(),
            "sel".to_owned(),
            "layout 1".to_owned()
        ]
    );
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!("end {:?}", p.end().ok()));
        log.push(format!("after {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
        log.push(format!("caret {:?}", p.selection().ok()));
    });
    assert_eq!(
        d.logged(),
        vec![
            "before Some(\"abc\")".to_owned(),
            "end Some(8)".to_owned(),
            "after Some(\"héllo 😀\")".to_owned(),
            "caret Some((8, 8, false))".to_owned(),
        ]
    );
}

#[test]
fn direct_a_document_switch_before_tsf_read_any_text_reports_only_selection_and_layout() {
    let d = Direct::new("abc");
    d.host.switch_to("xyz!");
    d.store.document_replaced();
    assert_eq!(
        d.take_events(),
        vec!["sel".to_owned(), "layout 1".to_owned()]
    );
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        log.borrow_mut().push(format!("end {:?}", p.end().ok()));
    });
    assert_eq!(d.logged(), vec!["end Some(4)".to_owned()]);
}

#[test]
fn direct_a_document_switch_drops_the_composition_without_committing_it() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::caret(2));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    let p = d.proxy.clone();
    d.locked_ok(RW_SYNC, move |_| {
        let _ = p.set_text(2, 2, "かな");
    });
    assert_eq!(d.host.overlay_now(), Some("かな".to_owned()));
    d.take_events();

    d.host.switch_to("xyz");
    d.store.document_replaced();

    assert!(!d.store.is_composing());
    let events = d.host.events_now();
    assert_eq!(events.last(), Some(&ImeEvent::End));
    let committed = events
        .iter()
        .any(|event| matches!(event, ImeEvent::Commit(_)));
    assert!(!committed, "{events:?}");
    assert_eq!(d.host.overlay_now(), None);
    assert_eq!(d.host.doc(), "xyz");
    // TSF saw "abかなcd": the composition string leaves together with the old text.
    assert_eq!(
        d.take_events(),
        vec![
            "text 0:6->3".to_owned(),
            "sel".to_owned(),
            "layout 1".to_owned()
        ]
    );
    // A late end of the composition has nothing to commit.
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert_eq!(d.host.doc(), "xyz");
    assert_eq!(d.host.selection_now(), Selection::caret(3));
}

#[test]
fn direct_a_document_switch_without_a_sink_still_drops_the_composition() {
    let _com = ComGuard::apartment_threaded().expect("COM initializes");
    let window = HiddenWindow::new();
    let host = TestHost::new("abcd");
    let store = TextStore::new(window.0, host.clone());
    host.move_caret(Selection::caret(2));
    let sink: ITfContextOwnerCompositionSink = store
        .interface()
        .cast()
        .expect("the store is a composition sink");
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    host.switch_to("xyz");
    store.document_replaced();
    assert!(!store.is_composing());
    assert_eq!(host.events_now(), vec![ImeEvent::Start, ImeEvent::End]);
    assert_eq!(host.doc(), "xyz");
}

// ---- real thread manager ------------------------------------------------------------------------

/// Wires the host to the store of `services`; with `repaint`, every host change also reports a
/// layout change, as an editor that repaints and scrolls on each IME event would.
fn wire(host: &Rc<TestHost>, services: &Rc<TextServices>, repaint: bool) {
    let weak = Rc::downgrade(services);
    host.link(move |note| {
        if let Some(services) = weak.upgrade() {
            match note {
                Note::Text {
                    start,
                    removed,
                    inserted,
                } => services.store().text_changed(start, &removed, &inserted),
                Note::Selection => services.store().selection_changed(),
            }
            if repaint {
                services.store().layout_changed();
            }
        }
    });
}

/// The real thread manager around the store, on a hidden window.
struct Tsf {
    services: Rc<TextServices>,
    host: Rc<TestHost>,
    window: HiddenWindow,
    _com: ComGuard,
}

impl Tsf {
    fn new(text: &str) -> Self {
        let com = ComGuard::apartment_threaded().expect("COM initializes");
        let window = HiddenWindow::new();
        let host = TestHost::new(text);
        let services = Rc::new(TextServices::new(window.0, host.clone()).expect("TSF activates"));
        wire(&host, &services, false);
        services.set_focus();
        Self {
            services,
            host,
            window,
            _com: com,
        }
    }

    /// Runs `edit` as a synchronous edit session; returns the session HRESULT.
    fn session(
        &self,
        flags: TF_CONTEXT_EDIT_CONTEXT_FLAGS,
        edit: impl FnOnce(u32) -> windows::core::Result<()> + 'static,
    ) -> HRESULT {
        self.services
            .edit_session(flags | TF_ES_SYNC, edit)
            .expect("the edit session is requested")
    }

    fn context(&self) -> ITfContext {
        self.services.context().clone()
    }

    /// The whole text as TSF reads it through its own range.
    fn tsf_text(&self) -> String {
        let context = self.context();
        let out = Rc::new(RefCell::new(String::new()));
        let result = out.clone();
        let hr = self.session(TF_ES_READ, move |ec| {
            let range = com!(context.GetStart(ec))?;
            let end = com!(context.GetEnd(ec))?;
            com!(range.ShiftEndToRange(ec, &end, TF_ANCHOR_END))?;
            let mut buffer = vec![0u16; 256];
            let mut got = 0u32;
            com!(range.GetText(ec, 0, &mut buffer, &mut got))?;
            *result.borrow_mut() = String::from_utf16_lossy(&buffer[..got as usize]);
            Ok(())
        });
        assert!(hr.is_ok(), "read session: {hr:?}");
        out.borrow().clone()
    }
}

fn acp_extent(range: &ITfRange) -> windows::core::Result<(i32, i32)> {
    let acp: ITfRangeACP = range.cast()?;
    let (mut start, mut count) = (0i32, 0i32);
    com!(acp.GetExtent(&mut start, &mut count))?;
    Ok((start, start + count))
}

/// A TIP-like composition owner: ends the composition when TSF terminates it.
#[implement(ITfCompositionSink)]
struct TestCompositionSink {
    terminated: Rc<Cell<u32>>,
}

#[allow(non_snake_case)]
impl ITfCompositionSink_Impl for TestCompositionSink_Impl {
    fn OnCompositionTerminated(
        &self,
        ecwrite: u32,
        pcomposition: Ref<ITfComposition>,
    ) -> windows::core::Result<()> {
        self.terminated.set(self.terminated.get() + 1);
        if let Some(composition) = pcomposition.as_ref() {
            com!(composition.EndComposition(ecwrite))?;
        }
        Ok(())
    }
}

type Held = Rc<RefCell<Option<ITfComposition>>>;

fn current(held: &Held) -> windows::core::Result<ITfComposition> {
    held.borrow()
        .clone()
        .ok_or_else(|| windows::core::Error::from(E_POINTER))
}

/// What a text service does: owns the current composition and edits it in sessions.
struct Tip {
    terminated: Rc<Cell<u32>>,
    composition: Held,
}

impl Tip {
    fn new() -> Self {
        Self {
            terminated: Rc::default(),
            composition: Rc::default(),
        }
    }

    /// Starts a composition over the ACP range `acp` and sets its text.
    fn start(&self, t: &Tsf, acp: (i32, i32), text: &str) {
        self.start_styled(t, acp, text, &[]);
    }

    /// [`Tip::start`], and gives unit `i` of the string the display attribute whose GUID atom
    /// is `atoms[i]` (`GUID_PROP_ATTRIBUTE`), all in the one edit session.
    fn start_styled(&self, t: &Tsf, acp: (i32, i32), text: &str, atoms: &[u32]) {
        let (context, terminated, held) = (
            t.context(),
            self.terminated.clone(),
            self.composition.clone(),
        );
        let (text, atoms) = (units(text), atoms.to_vec());
        let hr = t.session(TF_ES_READWRITE, move |ec| {
            let range = com!(context.GetStart(ec))?;
            let mut moved = 0i32;
            com!(range.ShiftEnd(ec, acp.1, &mut moved, std::ptr::null()))?;
            com!(range.ShiftStart(ec, acp.0, &mut moved, std::ptr::null()))?;
            let sink: ITfCompositionSink = TestCompositionSink { terminated }.into();
            let owner: ITfContextComposition = context.cast()?;
            let started = com!(owner.StartComposition(ec, &range, &sink))?;
            com!(range.SetText(ec, 0, &text))?;
            if !atoms.is_empty() {
                let property = com!(context.GetProperty(&GUID_PROP_ATTRIBUTE))?;
                for (index, atom) in atoms.iter().enumerate() {
                    // The one-unit range at `index` of the composition string.
                    let unit = com!(range.Clone())?;
                    let index = i32::try_from(index).expect("a short composition");
                    com!(unit.Collapse(ec, TF_ANCHOR_START))?;
                    com!(unit.ShiftEnd(ec, index + 1, &mut moved, std::ptr::null()))?;
                    com!(unit.ShiftStart(ec, index, &mut moved, std::ptr::null()))?;
                    let value = VARIANT::from(i32::try_from(*atom).expect("a small atom"));
                    com!(property.SetValue(ec, &unit, &value))?;
                }
            }
            *held.borrow_mut() = Some(started);
            Ok(())
        });
        assert!(hr.is_ok(), "start composition: {hr:?}");
    }

    /// Replaces the whole composition string.
    fn set_text(&self, t: &Tsf, text: &str) {
        let (held, text) = (self.composition.clone(), units(text));
        let hr = t.session(TF_ES_READWRITE, move |ec| {
            let range = com!(current(&held)?.GetRange())?;
            com!(range.SetText(ec, 0, &text))
        });
        assert!(hr.is_ok(), "set composition text: {hr:?}");
    }

    fn end(&self, t: &Tsf) {
        let held = self.composition.clone();
        let hr = t.session(TF_ES_READWRITE, move |ec| {
            let started = held
                .borrow_mut()
                .take()
                .ok_or_else(|| windows::core::Error::from(E_POINTER))?;
            com!(started.EndComposition(ec))
        });
        assert!(hr.is_ok(), "end composition: {hr:?}");
    }
}

/// Messages TSF or COM posted to this thread are dispatched, as the application's loop would.
fn pump(milliseconds: u64) {
    let until = std::time::Instant::now() + std::time::Duration::from_millis(milliseconds);
    while std::time::Instant::now() < until {
        let mut message = MSG::default();
        // SAFETY: standard non-blocking message pump of the thread that owns the window.
        while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
            // SAFETY: `message` was filled in by PeekMessageW.
            unsafe {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[test]
fn tsf_activates_pushes_the_context_and_reads_the_store_status() {
    let t = Tsf::new("abc");
    let status = com!(t.context().GetStatus()).expect("ITfContext::GetStatus");
    assert_eq!(status.dwDynamicFlags, 0);
    assert_eq!(status.dwStaticFlags, TS_SS_NOHIDDENTEXT);
    t.host.set_read_only(true);
    t.services.store().status_changed();
    let status = com!(t.context().GetStatus()).expect("ITfContext::GetStatus");
    assert_eq!(status.dwDynamicFlags, TS_SD_READONLY);
}

#[test]
fn tsf_insert_at_selection_lands_in_the_host_incl_surrogate_pairs() {
    let t = Tsf::new("");
    for text in ["TSF-ins", "\u{1F600}"] {
        let (context, text) = (t.context(), units(text));
        let hr = t.session(TF_ES_READWRITE, move |ec| {
            let insert: ITfInsertAtSelection = context.cast()?;
            com!(insert.InsertTextAtSelection(ec, INSERT_TEXT_AT_SELECTION_FLAGS(0), &text))?;
            Ok(())
        });
        assert!(hr.is_ok(), "{hr:?}");
    }
    assert_eq!(t.host.doc(), "TSF-ins\u{1F600}");
    assert_eq!(t.host.selection_now(), Selection::caret(11));
    assert_eq!(t.tsf_text(), "TSF-ins\u{1F600}");
    assert!(!t.services.store().is_composing());
    assert_eq!(t.host.overlay_now(), None);
    // TSF wraps the insertion in a composition: the host saw it start and end, the committed
    // text arrived verbatim and nothing is left over.
    let events = t.host.events_now();
    assert_eq!(events.last(), Some(&ImeEvent::End));
    let committed: String = events
        .iter()
        .filter_map(|event| match event {
            ImeEvent::Commit(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(committed, "TSF-ins\u{1F600}");
}

#[test]
fn tsf_range_set_text_and_set_selection_edit_through_the_store() {
    let t = Tsf::new("hello \u{1F600}");
    let context = t.context();
    let hr = t.session(TF_ES_READWRITE, {
        let context = context.clone();
        move |ec| {
            let range = com!(context.GetStart(ec))?;
            let end = com!(context.GetEnd(ec))?;
            com!(range.ShiftEndToRange(ec, &end, TF_ANCHOR_END))?;
            com!(range.SetText(ec, 0, &units("repl-\u{011F}\u{1F600}")))?;
            Ok(())
        }
    });
    assert!(hr.is_ok(), "{hr:?}");
    assert_eq!(t.host.doc(), "repl-\u{011F}\u{1F600}");
    let hr = t.session(TF_ES_READWRITE, move |ec| {
        let range = com!(context.GetStart(ec))?;
        let selection = TF_SELECTION {
            range: std::mem::ManuallyDrop::new(Some(range)),
            style: TF_SELECTIONSTYLE {
                ase: TF_AE_END,
                fInterimChar: false.into(),
            },
        };
        com!(context.SetSelection(ec, &[selection]))
    });
    assert!(hr.is_ok(), "{hr:?}");
    assert_eq!(t.host.selection_now(), Selection::caret(0));
}

#[test]
fn tsf_reads_host_edits_after_the_store_notified_it() {
    let t = Tsf::new("abc");
    assert_eq!(t.tsf_text(), "abc");
    // Typing: the host edits and notifies; TSF's next read sees the new text and caret.
    t.host.edit(3..3, "\u{011F}\u{1F600}");
    assert_eq!(t.tsf_text(), "abc\u{011F}\u{1F600}");
    let context = t.context();
    let out = Rc::new(Cell::new((0, 0)));
    let result = out.clone();
    let hr = t.session(TF_ES_READ, move |ec| {
        let mut selection = [TF_SELECTION::default()];
        let mut fetched = 0u32;
        com!(context.GetSelection(ec, TS_DEFAULT_SELECTION, &mut selection, &mut fetched))?;
        let range = selection[0]
            .range
            .as_ref()
            .ok_or_else(|| windows::core::Error::from(E_POINTER))?;
        result.set(acp_extent(range)?);
        Ok(())
    });
    assert!(hr.is_ok(), "{hr:?}");
    // ğ is one unit, the emoji two: the caret is at ACP 6.
    assert_eq!(out.get(), (6, 6));
}

#[test]
fn tsf_layout_and_hit_testing_reach_the_host_through_the_view() {
    let t = Tsf::new("ab\ncd\u{1F600}");
    let context = t.context();
    let out: Rc<RefCell<Vec<String>>> = Rc::default();
    let log = out.clone();
    let point = t.window.screen_point(34.0, 30.0);
    let hr = t.session(TF_ES_READ, move |ec| {
        let view = com!(context.GetActiveView())?;
        // "cd" on the second line: ACPs 3..5.
        let line = com!(context.GetStart(ec))?;
        let mut moved = 0i32;
        com!(line.ShiftEnd(ec, 5, &mut moved, std::ptr::null()))?;
        com!(line.ShiftStart(ec, 3, &mut moved, std::ptr::null()))?;
        let mut rect = RECT::default();
        let mut clipped = BOOL(0);
        com!(view.GetTextExt(ec, &line, &mut rect, &mut clipped))?;
        log.borrow_mut().push(format!(
            "ext {} {} {} {} {}",
            rect.left,
            rect.top,
            rect.right,
            rect.bottom,
            clipped.as_bool()
        ));
        let screen = com!(view.GetScreenExt())?;
        log.borrow_mut().push(format!(
            "screen {} {} {} {}",
            screen.left, screen.top, screen.right, screen.bottom
        ));
        let at = com!(view.GetRangeFromPoint(ec, &point, 0))?;
        log.borrow_mut()
            .push(format!("point {:?}", acp_extent(&at)?));
        Ok(())
    });
    assert!(hr.is_ok(), "{hr:?}");
    let logged = out.borrow().clone();
    let r = t.window.screen_rect(Rect::new(4.0, 26.0, 20.0, 20.0));
    assert_eq!(
        logged[0],
        format!("ext {} {} {} {} false", r.left, r.top, r.right, r.bottom)
    );
    let s = t.window.screen_rect(Rect::new(0.0, 0.0, 300.0, 100.0));
    assert_eq!(
        logged[1],
        format!("screen {} {} {} {}", s.left, s.top, s.right, s.bottom)
    );
    // The point behind the emoji is ACP 7: a collapsed range there.
    assert_eq!(logged[2], "point (7, 7)");
}

#[test]
fn tsf_composition_lifecycle_reaches_the_host_as_ime_events() {
    let t = Tsf::new("ab");
    let tip = Tip::new();
    t.host.move_caret(Selection::caret(1));
    tip.start(&t, (1, 1), "k");
    assert!(t.services.store().is_composing());
    assert_eq!(t.host.events_now().first(), Some(&ImeEvent::Start));
    assert_eq!(t.host.overlay_now(), Some("k".to_owned()));
    // The host document does not contain the composition string, TSF's text does.
    assert_eq!(t.host.doc(), "ab");
    assert_eq!(t.tsf_text(), "akb");

    tip.set_text(&t, "か");
    assert_eq!(t.host.overlay_now(), Some("か".to_owned()));
    assert_eq!(t.tsf_text(), "aかb");
    tip.set_text(&t, "かな");
    assert_eq!(t.tsf_text(), "aかなb");
    let events = t.host.events_now();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ImeEvent::Update { text, .. } if text == "かな")),
        "{events:?}"
    );

    tip.end(&t);
    assert!(!t.services.store().is_composing());
    assert_eq!(t.host.doc(), "aかなb");
    assert_eq!(t.host.overlay_now(), None);
    let events = t.host.events_now();
    assert_eq!(
        events[events.len() - 2..],
        [ImeEvent::Commit("かな".to_owned()), ImeEvent::End]
    );
    assert_eq!(t.tsf_text(), "aかなb");
}

#[test]
fn tsf_reconversion_composes_over_existing_text() {
    let t = Tsf::new("a漢字b");
    let tip = Tip::new();
    // The IME takes over "漢字" (ACPs 1..3) and offers the reading.
    tip.start(&t, (1, 3), "かんじ");
    assert_eq!(
        t.host.doc(),
        "ab",
        "the host gave up the text the composition replaces"
    );
    assert_eq!(t.host.overlay_now(), Some("かんじ".to_owned()));
    assert_eq!(t.tsf_text(), "aかんじb");
    tip.end(&t);
    assert_eq!(t.host.doc(), "aかんじb");
}

#[test]
fn tsf_composition_start_shift_commits_the_clause_and_composes_the_rest() {
    let t = Tsf::new("");
    let tip = Tip::new();
    tip.start(&t, (0, 0), "あい");
    let held = tip.composition.clone();
    let hr = t.session(TF_ES_READWRITE, move |ec| {
        // The IME commits "あ": the composition now starts after it.
        let started = current(&held)?;
        let range = com!(started.GetRange())?;
        let mut moved = 0i32;
        com!(range.Collapse(ec, TF_ANCHOR_START))?;
        com!(range.ShiftEnd(ec, 1, &mut moved, std::ptr::null()))?;
        com!(range.Collapse(ec, TF_ANCHOR_END))?;
        com!(started.ShiftStart(ec, &range))
    });
    assert!(hr.is_ok(), "{hr:?}");
    assert_eq!(t.host.doc(), "あ");
    assert_eq!(t.host.overlay_now(), Some("い".to_owned()));
    assert_eq!(t.tsf_text(), "あい");
    tip.end(&t);
    assert_eq!(t.host.doc(), "あい");
}

#[test]
fn tsf_terminating_the_composition_commits_it() {
    let t = Tsf::new("ab");
    let tip = Tip::new();
    t.host.move_caret(Selection::caret(2));
    tip.start(&t, (2, 2), "x");
    assert_eq!(t.host.overlay_now(), Some("x".to_owned()));
    // The host moves the caret by mouse: it asks the text service to finish first.
    t.services
        .terminate_composition()
        .expect("TerminateComposition");
    assert_eq!(tip.terminated.get(), 1, "the owner was asked to terminate");
    assert!(!t.services.store().is_composing());
    assert_eq!(t.host.doc(), "abx");
    assert_eq!(t.host.overlay_now(), None);
}

#[test]
fn tsf_a_document_switch_hands_text_services_the_text_of_the_new_document() {
    let t = Tsf::new("first");
    assert_eq!(t.tsf_text(), "first");
    t.host.switch_to("second text");
    t.services.document_replaced();
    assert_eq!(t.tsf_text(), "second text");
    // Edits of the new document are followed from there.
    t.host.edit(6..6, "!");
    assert_eq!(t.tsf_text(), "second! text");
}

#[test]
fn tsf_a_document_switch_ends_the_composition_without_committing_it_to_the_new_document() {
    let t = Tsf::new("ab");
    let tip = Tip::new();
    t.host.move_caret(Selection::caret(2));
    tip.start(&t, (2, 2), "x");
    assert_eq!(t.host.overlay_now(), Some("x".to_owned()));
    assert_eq!(t.tsf_text(), "abx");

    t.host.switch_to("new");
    t.services.document_replaced();

    assert_eq!(tip.terminated.get(), 1, "the service was asked to end");
    assert!(!t.services.store().is_composing());
    assert_eq!(t.host.doc(), "new", "nothing was committed");
    assert_eq!(t.host.overlay_now(), None);
    let events = t.host.events_now();
    let committed = events
        .iter()
        .any(|event| matches!(event, ImeEvent::Commit(_)));
    assert!(!committed, "{events:?}");
    assert_eq!(t.tsf_text(), "new");
}

#[test]
fn tsf_composition_updates_do_not_reschedule_themselves() {
    // Regression: refreshing the composition from a notification that itself schedules a
    // refresh looped forever, and because posted messages outrank input in GetMessage the
    // keyboard starved. Updates reach the host once per edit and then nothing happens, even
    // when the host reacts to every IME event with selection and layout notifications.
    let t = Tsf::new("ab");
    let tip = Tip::new();
    t.host.move_caret(Selection::caret(1));
    wire(&t.host, &t.services, true);
    tip.start(&t, (1, 1), "k");
    tip.set_text(&t, "か");
    let events_after_edits = t.host.events_now().len();
    let text_after_edits = t.tsf_text();
    pump(300);
    assert_eq!(
        t.host.events_now().len(),
        events_after_edits,
        "no IME event without an edit"
    );
    assert_eq!(t.tsf_text(), text_after_edits);
    tip.end(&t);
    let events_after_commit = t.host.events_now().len();
    pump(300);
    assert_eq!(t.host.events_now().len(), events_after_commit);
    assert_eq!(t.host.doc(), "aかb");
}

#[test]
fn tsf_teardown_and_recreation_on_one_thread_work() {
    let com = ComGuard::apartment_threaded().expect("COM initializes");
    let window = HiddenWindow::new();
    for _ in 0..2 {
        let host = TestHost::new("abc");
        let services = TextServices::new(window.0, host.clone()).expect("TSF activates");
        services.set_focus();
        assert!(!services.store().is_composing());
        drop(services);
    }
    drop(window);
    drop(com);
}

#[test]
fn tsf_activation_on_a_thread_without_com_fails_with_the_failed_call() {
    let outcome = std::thread::spawn(|| {
        let host = TestHost::new("abc");
        TextServices::new(HWND::default(), host)
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .join()
    .expect("the thread finishes");
    let message = outcome.expect_err("TSF cannot activate without COM");
    assert!(
        message.starts_with("CoCreateInstance(TF_ThreadMgr) failed"),
        "{message}"
    );
}

#[test]
fn tsf_debug_output_names_the_client_and_the_store() {
    let t = Tsf::new("ab");
    let text = format!("{:?}", t.services);
    assert!(text.starts_with("TextServices { client_id: "), "{text}");
    assert!(
        text.ends_with(", store: TextStore { composing: false, .. }, .. }"),
        "{text}"
    );
}

// ---- edges of the COM shell --------------------------------------------------------------------

#[test]
fn direct_the_debug_output_reports_whether_a_composition_is_active() {
    let d = Direct::new("ab");
    assert_eq!(
        format!("{:?}", d.store),
        "TextStore { composing: false, .. }"
    );
    assert!(
        com!(
            d.composition_sink()
                .OnStartComposition(None::<&ITfCompositionView>)
        )
        .expect("start")
        .as_bool()
    );
    assert_eq!(
        format!("{:?}", d.store),
        "TextStore { composing: true, .. }"
    );
}

#[test]
fn direct_an_empty_replacement_deletes_and_empty_or_null_arguments_are_handled() {
    let d = Direct::new("abcd");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        // An empty replacement text deletes the range.
        log.push(format!(
            "delete {:?}",
            p.set_text(1, 3, "")
                .map(|c| (c.acpStart, c.acpOldEnd, c.acpNewEnd))
        ));
        log.push(format!("text {:?}", p.text(0, -1, 16).map(|t| t.0).ok()));
        // No selection at all is not one selection.
        log.push(format!("none {:x}", code(com!(p.0.SetSelection(&[]))).0));
        // The wrapper takes slices; a count with a null array needs the raw vtable.
        let hr = com!((Interface::vtable(&p.0).SetSelection)(
            Interface::as_raw(&p.0),
            1,
            std::ptr::null()
        ));
        log.push(format!("null array {:x}", hr.0));
        // No place for the count of fetched selections.
        let mut selection = [TS_SELECTION_ACP::default(); 1];
        log.push(format!(
            "null fetched {:x}",
            code(com!(p.0.GetSelection(
                TS_DEFAULT_SELECTION,
                &mut selection,
                std::ptr::null_mut()
            )))
            .0
        ));
    });
    assert_eq!(
        d.logged(),
        vec![
            "delete Ok((1, 3, 1))".to_owned(),
            "text Some(\"ad\")".to_owned(),
            format!("none {:x}", E_INVALIDARG.0),
            format!("null array {:x}", E_POINTER.0),
            format!("null fetched {:x}", E_POINTER.0),
        ]
    );
    assert_eq!(d.host.doc(), "ad");
    // Neither the attribute request nor the point query accept null out parameters.
    let mut values = [TS_ATTRVAL::default()];
    assert_eq!(
        code(com!(
            d.proxy
                .0
                .RetrieveRequestedAttrs(&mut values, std::ptr::null_mut())
        )),
        E_POINTER
    );
    assert_eq!(
        code(com!(d.proxy.0.GetACPFromPoint(0, std::ptr::null(), 0))),
        E_INVALIDARG
    );
}

#[test]
fn direct_the_input_scope_rejects_null_out_pointers() {
    let d = Direct::new("abc");
    com!(d.proxy.0.RequestSupportedAttrs(0, &[])).expect("request");
    let mut values = [TS_ATTRVAL::default()];
    let mut fetched = 0u32;
    com!(d.proxy.0.RetrieveRequestedAttrs(&mut values, &mut fetched)).expect("Retrieve");
    assert_eq!(fetched, 1);
    let unknown = IUnknown::try_from(&values[0].varValue).expect("VT_UNKNOWN");
    let scope: ITfInputScope = unknown.cast().expect("ITfInputScope");
    let mut array: *mut InputScope = std::ptr::null_mut();
    let mut count = 7u32;
    assert_eq!(
        code(com!(scope.GetInputScopes(std::ptr::null_mut(), &mut count))),
        E_POINTER
    );
    assert_eq!(
        code(com!(scope.GetInputScopes(&mut array, std::ptr::null_mut()))),
        E_POINTER
    );
    assert!(array.is_null());
    assert_eq!(count, 7);
}

#[test]
fn direct_query_insert_skips_the_out_pointers_tsf_leaves_null() {
    let d = Direct::new("abcd");
    let (mut start, mut end) = (-1i32, -1i32);
    com!(
        d.proxy
            .0
            .QueryInsert(1, 3, 2, &mut start, std::ptr::null_mut())
    )
    .expect("QueryInsert");
    assert_eq!((start, end), (1, -1));
    com!(
        d.proxy
            .0
            .QueryInsert(1, 3, 2, std::ptr::null_mut(), &mut end)
    )
    .expect("QueryInsert");
    assert_eq!((start, end), (1, 3));
}

#[test]
fn direct_a_sink_without_the_sink_interface_and_no_sink_at_all_are_refused() {
    let d = Direct::new("abc");
    // The store is a COM object all right, but no ITextStoreACPSink.
    let foreign: IUnknown = d.proxy.0.cast().expect("IUnknown");
    assert_eq!(
        code(com!(d.proxy.0.AdviseSink(
            &ITextStoreACPSink::IID,
            &foreign,
            TS_AS_TEXT_CHANGE
        ))),
        E_NOINTERFACE
    );
    assert_eq!(
        code(com!(d.proxy.0.UnadviseSink(None::<&IUnknown>))),
        E_POINTER
    );
    // Both refusals left the advised sink alone: it still hears the host.
    d.host.edit(0..0, "Z");
    assert_eq!(
        d.take_events(),
        vec![
            "text 0:0->1".to_owned(),
            "sel".to_owned(),
            "layout 1".to_owned()
        ]
    );
}

#[test]
fn direct_reading_runs_only_or_an_empty_range_returns_no_characters_and_no_run() {
    let d = Direct::new("abc");
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(READ_SYNC, move |_| {
        let mut log = log.borrow_mut();
        // A buffer without capacity asks for runs only: no characters, no run, and the position
        // a read would go on at.
        log.push(format!("no buffer {:?}", p.text(1, -1, 0).ok()));
        // An empty range has no characters, so no run either, however much room there is.
        log.push(format!("empty {:?}", p.text(2, 2, 8).ok()));
        log.push(format!("filled {:?}", p.text(1, 3, 8).ok()));
    });
    assert_eq!(
        d.logged(),
        vec![
            "no buffer Some((\"\", 1, 0))".to_owned(),
            "empty Some((\"\", 2, 0))".to_owned(),
            "filled Some((\"bc\", 3, 1))".to_owned(),
        ]
    );
}

#[test]
fn direct_inserting_text_that_is_no_utf16_or_over_a_selection_across_the_composition_fails() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::new(1, 3));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        let (mut start, mut end) = (0i32, 0i32);
        let mut change = TS_TEXTCHANGE::default();
        // A lone surrogate is no text.
        log.push(format!(
            "lone {:x}",
            code(com!(p.0.InsertTextAtSelection(
                0,
                &[0xD800],
                &mut start,
                &mut end,
                &mut change
            )))
            .0
        ));
        // The selection reaches from the host text into the composition: there is no one place
        // to put the text.
        log.push(format!("select {:?}", p.set_selection(0, 2, false).is_ok()));
        log.push(format!("straddle {:x}", code(p.insert(0, "x")).0));
        // The failed insertions changed nothing.
        log.push(format!("text {:?}", p.text(0, -1, 100).map(|t| t.0).ok()));
    });
    assert_eq!(
        d.logged(),
        vec![
            format!("lone {:x}", E_INVALIDARG.0),
            "select true".to_owned(),
            format!("straddle {:x}", TS_E_INVALIDPOS.0),
            "text Some(\"abcd\")".to_owned(),
        ]
    );
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert_eq!(d.host.doc(), "abcd");
}

#[test]
fn direct_a_backward_composition_selection_is_a_reversed_host_selection_after_the_commit() {
    let d = Direct::new("abcd");
    d.host.move_caret(Selection::caret(2));
    let sink = d.composition_sink();
    assert!(
        com!(sink.OnStartComposition(None::<&ITfCompositionView>))
            .expect("start")
            .as_bool()
    );
    let (p, log) = (d.proxy.clone(), d.log.clone());
    d.locked_ok(RW_SYNC, move |_| {
        let mut log = log.borrow_mut();
        log.push(format!("set {:?}", p.set_text(2, 2, "かな").is_ok()));
        // "な" backward: TSF's caret is at its start.
        log.push(format!("select {:?}", p.set_selection(3, 4, true).is_ok()));
    });
    assert_eq!(
        d.logged(),
        vec!["set true".to_owned(), "select true".to_owned()]
    );
    com!(sink.OnEndComposition(None::<&ITfCompositionView>)).expect("end");
    assert_eq!(d.host.doc(), "abかなcd");
    // ACPs 3..4 are the bytes 5..8 of the committed text; the head is at the start.
    assert_eq!(d.host.selection_now(), Selection::new(8, 5));
}

#[test]
fn tsf_end_edit_without_a_context_or_with_a_bad_cookie_leaves_the_composition_alone() {
    let t = Tsf::new("ab");
    let tip = Tip::new();
    t.host.move_caret(Selection::caret(1));
    tip.start(&t, (1, 1), "k");
    let before = t.host.events_now();
    let sink: ITfTextEditSink = t
        .services
        .store()
        .interface()
        .cast()
        .expect("the store is a text edit sink");
    // No context: there is nothing to read the display attributes from.
    com!(sink.OnEndEdit(None::<&ITfContext>, 0, None::<&ITfEditRecord>)).expect("ignored");
    // A context but the cookie of no edit: reading fails and the composition stays as it was.
    com!(sink.OnEndEdit(&t.context(), 0, None::<&ITfEditRecord>)).expect("ignored");
    assert!(t.services.store().is_composing());
    assert_eq!(t.host.events_now(), before);
    // Without a composition the call returns before it touches TSF.
    tip.end(&t);
    com!(sink.OnEndEdit(&t.context(), 0, None::<&ITfEditRecord>)).expect("ignored");
    assert_eq!(t.host.doc(), "akb");
}

// ---- display attributes through fake system managers -------------------------------------------

/// The display attribute of each GUID a fake `ITfDisplayAttributeMgr` knows.
type Attributes = Rc<Vec<(GUID, TF_DA_ATTR_INFO)>>;

/// The `ITfDisplayAttributeInfo` of one GUID.
#[implement(ITfDisplayAttributeInfo)]
struct FakeAttributeInfo {
    guid: GUID,
    attribute: TF_DA_ATTR_INFO,
}

#[allow(non_snake_case)]
impl ITfDisplayAttributeInfo_Impl for FakeAttributeInfo_Impl {
    fn GetGUID(&self) -> windows::core::Result<GUID> {
        Ok(self.guid)
    }

    fn GetDescription(&self) -> windows::core::Result<BSTR> {
        Err(E_NOTIMPL.into())
    }

    fn GetAttributeInfo(&self, pda: *mut TF_DISPLAYATTRIBUTE) -> windows::core::Result<()> {
        let info = TF_DISPLAYATTRIBUTE {
            bAttr: self.attribute,
            ..TF_DISPLAYATTRIBUTE::default()
        };
        // SAFETY: the caller passes a valid out pointer.
        unsafe { pda.write(info) };
        Ok(())
    }

    fn SetAttributeInfo(&self, _pda: *const TF_DISPLAYATTRIBUTE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

/// An `ITfDisplayAttributeMgr` that knows exactly the GUIDs of its table.
#[implement(ITfDisplayAttributeMgr)]
struct FakeDisplayMgr {
    attributes: Attributes,
}

#[allow(non_snake_case)]
impl ITfDisplayAttributeMgr_Impl for FakeDisplayMgr_Impl {
    fn OnUpdateInfo(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn EnumDisplayAttributeInfo(&self) -> windows::core::Result<IEnumTfDisplayAttributeInfo> {
        Err(E_NOTIMPL.into())
    }

    fn GetDisplayAttributeInfo(
        &self,
        guid: *const GUID,
        ppinfo: OutRef<ITfDisplayAttributeInfo>,
        pclsidowner: *mut GUID,
    ) -> windows::core::Result<()> {
        // SAFETY: the caller passes a valid GUID.
        let guid = unsafe { *guid };
        let attribute = self
            .attributes
            .iter()
            .find(|(known, _)| *known == guid)
            .map(|(_, attribute)| *attribute)
            .ok_or_else(|| windows::core::Error::from(E_INVALIDARG))?;
        if !pclsidowner.is_null() {
            // SAFETY: a non-null owner pointer is valid for one write.
            unsafe { pclsidowner.write(GUID::zeroed()) };
        }
        ppinfo.write(Some(FakeAttributeInfo { guid, attribute }.into()))
    }
}

/// The class object of a fake system manager: creates a [`FakeDisplayMgr`], or fails when it
/// has no table.
#[implement(IClassFactory)]
struct FakeFactory {
    attributes: Option<Attributes>,
}

#[allow(non_snake_case)]
impl IClassFactory_Impl for FakeFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if !punkouter.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let Some(attributes) = &self.attributes else {
            return Err(E_FAIL.into());
        };
        let manager: IUnknown = FakeDisplayMgr {
            attributes: attributes.clone(),
        }
        .into();
        // SAFETY: COM passes a valid IID and out pointer.
        unsafe { manager.query(riid, ppvobject) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

/// A fake class object registered in this process for `clsid`, so that
/// `CoCreateInstance(clsid, CLSCTX_INPROC_SERVER)` on this apartment creates the fake instead of
/// the system object. Nothing is written to the registry; it is revoked on drop.
struct RegisteredClass(u32);

impl RegisteredClass {
    fn new(clsid: GUID, attributes: Option<Attributes>) -> Self {
        let factory: IClassFactory = FakeFactory { attributes }.into();
        let unknown: IUnknown = factory.cast().expect("the factory is an IUnknown");
        let cookie = com!(CoRegisterClassObject(
            &clsid,
            &unknown,
            CLSCTX_INPROC_SERVER,
            REGCLS_MULTIPLEUSE
        ))
        .expect("the class object is registered");
        Self(cookie)
    }
}

impl Drop for RegisteredClass {
    fn drop(&mut self) {
        let _ = com!(CoRevokeClassObject(self.0));
    }
}

/// The attribute runs of the last `ImeEvent::Update` the host received.
fn last_attributes(events: &[ImeEvent]) -> Vec<(Range<usize>, ImeAttrStyle)> {
    events
        .iter()
        .rev()
        .find_map(|event| match event {
            ImeEvent::Update { attrs, .. } => Some(
                attrs
                    .iter()
                    .map(|attr| (attr.range.clone(), attr.style))
                    .collect(),
            ),
            _ => None,
        })
        .expect("the host received a composition update")
}

#[test]
fn tsf_display_attributes_of_the_text_service_reach_the_host_as_styled_runs() {
    let t = Tsf::new("");
    let known = [
        (
            GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000001),
            TF_ATTR_TARGET_CONVERTED,
        ),
        (
            GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000002),
            TF_ATTR_CONVERTED,
        ),
        (
            GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000003),
            TF_ATTR_TARGET_NOTCONVERTED,
        ),
        (
            GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000004),
            TF_ATTR_INPUT_ERROR,
        ),
        (
            GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000005),
            TF_ATTR_FIXEDCONVERTED,
        ),
        (
            GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000006),
            TF_ATTR_INPUT,
        ),
    ];
    // A GUID the manager does not know: its run carries no style.
    let unknown = GUID::from_u128(0x7a1c0001_5e5b_4f0c_9d3e_000000000007);
    let _manager =
        RegisteredClass::new(CLSID_TF_DisplayAttributeMgr, Some(Rc::new(known.to_vec())));
    let categories: ITfCategoryMgr = com!(CoCreateInstance(
        &CLSID_TF_CategoryMgr,
        None::<&IUnknown>,
        CLSCTX_INPROC_SERVER
    ))
    .expect("the category manager");
    let atoms: Vec<u32> = known
        .iter()
        .map(|(guid, _)| *guid)
        .chain([unknown])
        .map(|guid| com!(categories.RegisterGUID(&guid)).expect("a GUID atom"))
        .collect();
    let tip = Tip::new();
    tip.start_styled(&t, (0, 0), "abcdefg", &atoms);
    assert_eq!(
        last_attributes(&t.host.events_now()),
        vec![
            (0..1, ImeAttrStyle::TargetConverted),
            (1..2, ImeAttrStyle::Converted),
            (2..3, ImeAttrStyle::TargetNotConverted),
            (3..4, ImeAttrStyle::InputError),
            (4..5, ImeAttrStyle::FixedConverted),
            (5..6, ImeAttrStyle::Input),
            // The unknown attribute leaves the last unit at the default.
            (6..7, ImeAttrStyle::Input),
        ]
    );
    assert_eq!(t.host.overlay_now(), Some("abcdefg".to_owned()));
    tip.end(&t);
    assert_eq!(t.host.doc(), "abcdefg");
}

#[test]
fn tsf_attribute_managers_that_cannot_be_created_leave_the_default_attributes() {
    // One initialization around both rounds: the rounds' own guards then only nest.
    let _com = ComGuard::apartment_threaded().expect("COM initializes");
    for failing in [CLSID_TF_CategoryMgr, CLSID_TF_DisplayAttributeMgr] {
        let t = Tsf::new("ab");
        let _manager = RegisteredClass::new(failing, None);
        let tip = Tip::new();
        t.host.move_caret(Selection::caret(1));
        tip.start(&t, (1, 1), "か");
        assert_eq!(
            last_attributes(&t.host.events_now()),
            vec![(0..3, ImeAttrStyle::Input)]
        );
        assert_eq!(t.host.overlay_now(), Some("か".to_owned()));
        tip.end(&t);
        assert_eq!(t.host.doc(), "aかb");
    }
}

// ---- failing system calls ----------------------------------------------------------------------

/// The call an error message names: "create window" for "create window failed: ...".
#[cfg(feature = "fault-injection")]
fn failed_call(message: &str) -> &str {
    message.split(" failed: ").next().unwrap_or_default()
}

#[cfg(feature = "fault-injection")]
#[test]
fn tsf_every_failing_call_of_the_activation_is_reported_by_its_step_and_leaves_nothing_behind() {
    let _com = ComGuard::apartment_threaded().expect("COM initializes");
    let window = HiddenWindow::new();
    let host = TestHost::new("abc");
    // Eight checked calls set TSF up and one takes it down: the ninth failure is the
    // teardown's, which the activation survives.
    let outcomes = ssw_win::fault::sweep(|| {
        TextServices::new(window.0, host.clone())
            .map(drop)
            .map_err(|error| error.to_string())
    });
    let steps: Vec<Option<&str>> = outcomes
        .iter()
        .map(|outcome| outcome.as_ref().err().map(|message| failed_call(message)))
        .collect();
    assert_eq!(
        steps,
        vec![
            Some("CoCreateInstance(TF_ThreadMgr)"),
            Some("ITfThreadMgr::Activate"),
            Some("QueryInterface(IUnknown)"),
            Some("ITfThreadMgr::CreateDocumentMgr"),
            Some("ITfDocumentMgr::CreateContext"),
            Some("QueryInterface(ITfSource)"),
            Some("ITfSource::AdviseSink(ITfTextEditSink)"),
            Some("ITfDocumentMgr::Push"),
            None,
        ]
    );
    // The failures left TSF usable: the next activation goes through and works.
    let services = TextServices::new(window.0, host.clone()).expect("TSF activates again");
    services.set_focus();
    assert!(!services.store().is_composing());
}

#[cfg(feature = "fault-injection")]
#[test]
fn tsf_a_failing_composition_services_query_leaves_the_composition_running() {
    let t = Tsf::new("ab");
    let tip = Tip::new();
    t.host.move_caret(Selection::caret(2));
    tip.start(&t, (2, 2), "x");
    ssw_win::fault::arm(0);
    let failed = t.services.terminate_composition();
    assert!(ssw_win::fault::tripped());
    ssw_win::fault::disarm();
    let message = failed.expect_err("the query fails").to_string();
    assert_eq!(
        failed_call(&message),
        "QueryInterface(ITfContextOwnerCompositionServices)"
    );
    // Nothing was asked of the text service: the composition goes on and ends as usual.
    assert!(t.services.store().is_composing());
    assert_eq!(tip.terminated.get(), 0);
    t.services
        .terminate_composition()
        .expect("TerminateComposition");
    assert_eq!(tip.terminated.get(), 1);
    assert_eq!(t.host.doc(), "abx");
}

#[cfg(feature = "fault-injection")]
#[test]
fn tsf_failing_calls_while_reading_a_composition_degrade_its_attributes_not_the_composition() {
    let _com = ComGuard::apartment_threaded().expect("COM initializes");
    let known = [
        (
            GUID::from_u128(0x7a1c0002_5e5b_4f0c_9d3e_000000000001),
            TF_ATTR_TARGET_CONVERTED,
        ),
        (
            GUID::from_u128(0x7a1c0002_5e5b_4f0c_9d3e_000000000002),
            TF_ATTR_CONVERTED,
        ),
    ];
    // Each call of the composition's start and its attribute read fails once, in turn; the run
    // that fails nothing is the last.
    let mut outcomes = Vec::new();
    for call in 0.. {
        // The order of the passing display attribute test: TSF is activated first, then the
        // fake manager takes the place of the system's.
        let t = Tsf::new("");
        let _manager =
            RegisteredClass::new(CLSID_TF_DisplayAttributeMgr, Some(Rc::new(known.to_vec())));
        let categories: ITfCategoryMgr = com!(CoCreateInstance(
            &CLSID_TF_CategoryMgr,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER
        ))
        .expect("the category manager");
        let atoms: Vec<u32> = known
            .iter()
            .map(|(guid, _)| com!(categories.RegisterGUID(guid)).expect("a GUID atom"))
            .collect();
        let tip = Tip::new();
        ssw_win::fault::arm(call);
        tip.start_styled(&t, (0, 0), "ab", &atoms);
        let hit = ssw_win::fault::tripped();
        ssw_win::fault::disarm();
        outcomes.push(last_attributes(&t.host.events_now()));
        tip.end(&t);
        assert_eq!(
            t.host.doc(),
            "ab",
            "call {call}: the composition commits whatever failed"
        );
        if !hit {
            break;
        }
    }
    let styled = vec![
        (0..1, ImeAttrStyle::TargetConverted),
        (1..2, ImeAttrStyle::Converted),
    ];
    assert_eq!(outcomes.last(), Some(&styled), "no failure, full styling");
    // Whatever failed, the host got attributes covering the whole string without gaps.
    for attrs in &outcomes {
        assert_eq!(attrs.first().map(|(range, _)| range.start), Some(0));
        assert_eq!(attrs.last().map(|(range, _)| range.end), Some(2));
        assert!(
            attrs
                .windows(2)
                .all(|pair| pair[0].0.end == pair[1].0.start),
            "{attrs:?}"
        );
    }
    // A property that cannot be read leaves the default style; a run that cannot be read only
    // costs that run its style.
    assert!(outcomes.contains(&vec![(0..2, ImeAttrStyle::Input)]));
    assert!(outcomes.contains(&vec![
        (0..1, ImeAttrStyle::TargetConverted),
        (1..2, ImeAttrStyle::Input)
    ]));
}
