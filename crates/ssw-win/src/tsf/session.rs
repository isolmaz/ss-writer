//! [`TextServices`]: the TSF thread manager, document manager and context of one window.

use std::cell::RefCell;
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::TextServices::{
    CLSID_TF_ThreadMgr, ITfCompositionView, ITfContext, ITfContextOwnerCompositionServices,
    ITfDocumentMgr, ITfEditSession, ITfEditSession_Impl, ITfSource, ITfTextEditSink, ITfThreadMgr,
    TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_POPF_ALL,
};
use windows::core::{HRESULT, IUnknown, Interface, Result, implement};

use super::{TextStore, TextStoreHost};
use crate::com::{OsResultExt, WinError, delivered};
use crate::fault::check;

/// The body of an edit session: runs once with the edit cookie.
type EditBody = Box<dyn FnOnce(u32) -> Result<()>>;

/// An edit session that runs a closure with the edit cookie.
#[implement(ITfEditSession)]
struct ClosureSession {
    run: RefCell<Option<EditBody>>,
}

#[allow(non_snake_case)]
impl ITfEditSession_Impl for ClosureSession_Impl {
    fn DoEditSession(&self, ec: u32) -> Result<()> {
        // Release the borrow before running user code, which may request another session.
        let run = self.run.borrow_mut().take();
        // A session TSF runs twice does nothing the second time.
        run.map_or(Ok(()), |run| run(ec))
    }
}

/// The thread manager that setup activates. Deactivates it when dropped, whichever way setup or
/// teardown ends.
struct ActiveThreadMgr(ITfThreadMgr);

impl Deref for ActiveThreadMgr {
    type Target = ITfThreadMgr;

    fn deref(&self) -> &ITfThreadMgr {
        &self.0
    }
}

impl Drop for ActiveThreadMgr {
    fn drop(&mut self) {
        // SAFETY: balances the `Activate` of setup; a failure leaves nothing to clean up.
        let _ = unsafe { self.0.Deactivate() };
    }
}

/// The TSF side of one window: activates the thread manager, owns the document manager and
/// context around a [`TextStore`], and associates the document manager with the window's focus.
///
/// Create it on the UI thread after COM was initialized as a single-threaded apartment
/// ([`ComGuard::apartment_threaded`](crate::com::ComGuard::apartment_threaded)) and drop it
/// before the guard and before the window is destroyed. Dropping tears everything down in
/// order: `TS_LC_DESTROY`, text-edit sink, focus association, context pop (TSF unadvises the
/// store), thread manager deactivation.
pub struct TextServices {
    // Fields drop in this order: the thread manager is deactivated first, after `Drop::drop`.
    thread_mgr: ActiveThreadMgr,
    store: TextStore,
    client_id: u32,
    doc_mgr: ITfDocumentMgr,
    context: ITfContext,
    edit_sink_cookie: u32,
    hwnd: HWND,
}

impl fmt::Debug for TextServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextServices")
            .field("client_id", &self.client_id)
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl TextServices {
    /// Activates TSF for `hwnd` with `host` as its editor. The document manager is associated
    /// with the window (TSF focuses it whenever the window gets keyboard focus); call
    /// [`TextServices::set_focus`] when the window already has focus.
    ///
    /// # Errors
    /// Fails when the thread manager, document manager or context cannot be created.
    pub fn new(hwnd: HWND, host: Rc<dyn TextStoreHost>) -> std::result::Result<Self, WinError> {
        // SAFETY: plain in-process activation of the system thread manager.
        let thread_mgr: ITfThreadMgr =
            check(unsafe { CoCreateInstance(&CLSID_TF_ThreadMgr, None, CLSCTX_INPROC_SERVER) })
                .ctx("CoCreateInstance(TF_ThreadMgr)")?;
        // From here on the guard deactivates the manager whichever way setup ends (its result
        // is ignored), so no activation outlives a failed `Activate` or `attach`.
        let thread_mgr = ActiveThreadMgr(thread_mgr);
        // SAFETY: plain call on the live thread manager.
        let client_id = check(unsafe { thread_mgr.Activate() }).ctx("ITfThreadMgr::Activate")?;
        let (store, doc_mgr, context, edit_sink_cookie) =
            Self::attach(&thread_mgr, client_id, hwnd, host)?;
        Ok(Self {
            store,
            thread_mgr,
            client_id,
            doc_mgr,
            context,
            edit_sink_cookie,
            hwnd,
        })
    }

    /// Creates the document manager and context for `store`, pushes it (TSF advises the
    /// store's sink) and associates it with the window.
    fn attach(
        thread_mgr: &ITfThreadMgr,
        client_id: u32,
        hwnd: HWND,
        host: Rc<dyn TextStoreHost>,
    ) -> std::result::Result<(TextStore, ITfDocumentMgr, ITfContext, u32), WinError> {
        let store = TextStore::new(hwnd, host);
        let unknown: IUnknown =
            check(store.interface().cast::<IUnknown>()).ctx("QueryInterface(IUnknown)")?;
        // SAFETY: plain calls on live interfaces; the out parameters are valid for the calls.
        unsafe {
            let doc_mgr =
                check(thread_mgr.CreateDocumentMgr()).ctx("ITfThreadMgr::CreateDocumentMgr")?;
            let mut context: Option<ITfContext> = None;
            let mut text_store_cookie = 0u32;
            let created =
                doc_mgr.CreateContext(client_id, 0, &unknown, &mut context, &mut text_store_cookie);
            let context =
                delivered(check(created), context).ctx("ITfDocumentMgr::CreateContext")?;
            // Our object is also the text-edit sink: it reads the IME's display attributes at
            // the end of every edit.
            let source: ITfSource =
                check(context.cast::<ITfSource>()).ctx("QueryInterface(ITfSource)")?;
            let edit_sink_cookie = check(source.AdviseSink(&ITfTextEditSink::IID, &unknown))
                .ctx("ITfSource::AdviseSink(ITfTextEditSink)")?;
            // TSF calls `ITextStoreACP2::AdviseSink` here.
            check(doc_mgr.Push(&context)).ctx("ITfDocumentMgr::Push")?;
            // The previous focus document manager is a null out parameter when the window had
            // none; windows-rs reports that as an "empty" error (HRESULT 0), not a failure.
            let _ = thread_mgr.AssociateFocus(hwnd, &doc_mgr);
            store.layout_created();
            Ok((store, doc_mgr, context, edit_sink_cookie))
        }
    }

    /// The text store, to report host changes to.
    #[must_use]
    pub fn store(&self) -> &TextStore {
        &self.store
    }

    /// Makes this window's document manager the thread's TSF focus. Call it when the window
    /// gets keyboard focus (or is created with focus).
    pub fn set_focus(&self) {
        // SAFETY: plain call on the live thread manager and document manager. A failure only
        // means TSF keeps the previous focus.
        let _ = unsafe { self.thread_mgr.SetFocus(&self.doc_mgr) };
    }

    /// The TSF context, for callers that run their own edit sessions.
    #[must_use]
    pub fn context(&self) -> &ITfContext {
        &self.context
    }

    /// Runs `edit` as an edit session of this context (`flags` pick sync/async and
    /// read/read-write, e.g. `TF_ES_SYNC | TF_ES_READWRITE`). Returns the session HRESULT:
    /// `S_OK` when it ran, `TF_E_SYNCHRONOUS` when TSF refused a synchronous session.
    ///
    /// # Errors
    /// Fails when TSF rejects the request itself.
    pub fn edit_session(
        &self,
        flags: TF_CONTEXT_EDIT_CONTEXT_FLAGS,
        edit: impl FnOnce(u32) -> Result<()> + 'static,
    ) -> std::result::Result<HRESULT, WinError> {
        let session: ITfEditSession = ClosureSession {
            run: RefCell::new(Some(Box::new(edit))),
        }
        .into();
        // SAFETY: plain call on the live context with a live session object.
        unsafe {
            self.context
                .RequestEditSession(self.client_id, &session, flags)
        }
        .ctx("ITfContext::RequestEditSession")
    }

    /// Asks the text service to commit the current composition, e.g. before the host moves
    /// the caret by mouse or replaces the document.
    ///
    /// # Errors
    /// Fails when TSF rejects the request.
    pub fn terminate_composition(&self) -> std::result::Result<(), WinError> {
        let services: ITfContextOwnerCompositionServices =
            check(self.context.cast::<ITfContextOwnerCompositionServices>())
                .ctx("QueryInterface(ITfContextOwnerCompositionServices)")?;
        // SAFETY: plain call on the live composition services; a null view terminates all.
        unsafe { services.TerminateComposition(None::<&ITfCompositionView>) }
            .ctx("ITfContextOwnerCompositionServices::TerminateComposition")
    }
}

impl Drop for TextServices {
    fn drop(&mut self) {
        self.store.layout_destroyed();
        // SAFETY: plain calls on live interfaces during teardown; failures leave nothing to
        // clean up (the window or thread is going away).
        unsafe {
            if let Ok(source) = check(self.context.cast::<ITfSource>()) {
                let _ = source.UnadviseSink(self.edit_sink_cookie);
            }
            let _ = self
                .thread_mgr
                .AssociateFocus(self.hwnd, None::<&ITfDocumentMgr>);
            let _ = self.doc_mgr.Pop(TF_POPF_ALL);
        }
        // The thread manager is deactivated when the `thread_mgr` field drops, after this.
    }
}
