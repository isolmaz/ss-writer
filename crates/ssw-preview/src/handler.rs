//! The preview handler COM object: what Explorer's preview pane talks to.
//!
//! One object implements `IPreviewHandler`, `IInitializeWithStream`, `IObjectWithSite`,
//! `IOleWindow` and `IPreviewHandlerVisuals`. Explorer drives it in a fixed order (all on one
//! apartment thread): `Initialize` with the file's stream, `SetWindow` with the host window and
//! rectangle, any number of `IPreviewHandlerVisuals` calls, `DoPreview`, then `SetRect`,
//! `SetFocus` and friends until `Unload`.
//!
//! Every method runs under a panic guard: Explorer must never see a Rust panic, which would
//! abort the process that hosts every preview. A failure shows up as an error code (Explorer then
//! shows its own "no preview" text) and never as a crash. A file that cannot be shown (too large,
//! unreadable stream) is not a failure: the pane says so with a localized notice.

use std::cell::{RefCell, RefMut};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use ssw_ui::editor::DocumentKind;
use ssw_win::com::WinError;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::locale::ui_language;
use ssw_win::theme::{Win32Settings, read_system_theme};
use ssw_win::wic::DecodedImage;
use windows::Win32::Foundation::{
    COLORREF, E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, ERROR_ALREADY_INITIALIZED,
    HWND, RECT, S_FALSE,
};
use windows::Win32::Graphics::Gdi::LOGFONTW;
use windows::Win32::System::Com::IStream;
use windows::Win32::System::Ole::{
    IObjectWithSite, IObjectWithSite_Impl, IOleWindow, IOleWindow_Impl,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IPreviewHandler, IPreviewHandler_Impl, IPreviewHandlerFrame, IPreviewHandlerVisuals,
    IPreviewHandlerVisuals_Impl,
};
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::core::{BOOL, Error, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement};

use crate::appearance::Visuals;
use crate::content::PreviewContent;
use crate::notice;
use crate::server::ObjectToken;
use crate::source::{MAX_PREVIEW_BYTES, decode_text, is_mermaid_name, read_stream, stream_name};
use crate::window::PreviewWindow;

/// Runs `body`, turning a panic into `E_UNEXPECTED`.
pub(crate) fn guarded<T>(body: impl FnOnce() -> Result<T>) -> Result<T> {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or_else(|_| Err(Error::from(E_UNEXPECTED)))
}

/// The `HRESULT` error a platform failure is reported as.
fn to_error(error: WinError) -> Error {
    match error {
        WinError::Os { source, .. } => source,
        WinError::DeviceLost { .. } | WinError::Invalid(_) => Error::from(E_FAIL),
    }
}

/// What the host has told the handler so far.
#[derive(Debug, Default)]
struct State {
    /// The text to show, from `Initialize`.
    text: Option<String>,
    /// Markdown, or a whole Mermaid diagram for a `.mmd` stream.
    kind: DocumentKind,
    /// The colors and font of `IPreviewHandlerVisuals`.
    visuals: Visuals,
    /// The host's site (`IObjectWithSite`); an `IPreviewHandlerFrame` among others.
    site: Option<IUnknown>,
    /// The host window from `SetWindow`.
    parent: Option<HWND>,
    /// Where the preview goes inside `parent`.
    rect: RECT,
    /// The preview, from `DoPreview` until `Unload`.
    window: Option<PreviewWindow>,
}

impl State {
    /// The host's `IPreviewHandlerFrame`, when the site is one.
    fn frame(&self) -> Option<IPreviewHandlerFrame> {
        self.site.as_ref()?.cast().ok()
    }

    /// Hands the current visuals to the showing preview.
    fn apply_visuals(&self) {
        if let Some(window) = &self.window {
            window.set_visuals(&self.visuals, &read_system_theme(&Win32Settings));
        }
    }
}

/// The preview handler object (CLSID [`crate::factory::CLSID_PREVIEW_HANDLER`]).
#[implement(
    IPreviewHandler,
    IInitializeWithStream,
    IObjectWithSite,
    IOleWindow,
    IPreviewHandlerVisuals
)]
pub(crate) struct PreviewHandler {
    state: RefCell<State>,
    /// Declared last: the DLL may unload once the token is gone, so it must drop after the
    /// preview window and everything else that has code in this DLL.
    _token: ObjectToken,
}

impl PreviewHandler {
    pub(crate) fn new() -> Self {
        Self {
            state: RefCell::default(),
            _token: ObjectToken::new(),
        }
    }

    /// The state, unless a call is already inside it (a host that re-enters the handler gets
    /// an error instead of a double borrow).
    fn state(&self) -> Result<RefMut<'_, State>> {
        self.state.try_borrow_mut().map_err(|_| Error::from(E_FAIL))
    }
}

#[allow(non_snake_case)]
impl IInitializeWithStream_Impl for PreviewHandler_Impl {
    /// Reads the whole file now (the stream is not kept) and decodes it. A stream that cannot
    /// be read or is over the size limit becomes a notice instead of the document.
    fn Initialize(&self, pstream: Ref<IStream>, _grfmode: u32) -> Result<()> {
        guarded(|| {
            let stream = pstream.ok()?;
            let mut state = self.state()?;
            if state.text.is_some() {
                return Err(Error::from(HRESULT::from_win32(
                    ERROR_ALREADY_INITIALIZED.0,
                )));
            }
            let (text, kind) = match read_stream(stream, MAX_PREVIEW_BYTES) {
                Ok(bytes) => {
                    let diagram = stream_name(stream).is_some_and(|name| is_mermaid_name(&name));
                    let kind = if diagram {
                        DocumentKind::Mermaid
                    } else {
                        DocumentKind::Markdown
                    };
                    (decode_text(&bytes), kind)
                }
                // The notice is a paragraph, whatever the file was.
                Err(error) => (
                    notice::for_error(&notice::localizer(&ui_language()), &error),
                    DocumentKind::Markdown,
                ),
            };
            state.text = Some(text);
            state.kind = kind;
            Ok(())
        })
    }
}

#[allow(non_snake_case)]
impl IPreviewHandler_Impl for PreviewHandler_Impl {
    fn SetWindow(&self, hwnd: HWND, prc: *const RECT) -> Result<()> {
        guarded(|| {
            // SAFETY: the host passes a pointer to a valid rectangle (or null, rejected here).
            let rect = unsafe { prc.as_ref() }
                .copied()
                .ok_or_else(|| Error::from(E_POINTER))?;
            if hwnd.is_invalid() {
                return Err(Error::from(E_INVALIDARG));
            }
            let mut state = self.state()?;
            state.parent = Some(hwnd);
            state.rect = rect;
            if let Some(window) = &state.window {
                window.set_parent(hwnd);
                window.set_rect(rect);
            }
            Ok(())
        })
    }

    fn SetRect(&self, prc: *const RECT) -> Result<()> {
        guarded(|| {
            // SAFETY: the host passes a pointer to a valid rectangle (or null, rejected here).
            let rect = unsafe { prc.as_ref() }
                .copied()
                .ok_or_else(|| Error::from(E_POINTER))?;
            let mut state = self.state()?;
            state.rect = rect;
            if let Some(window) = &state.window {
                window.set_rect(rect);
            }
            Ok(())
        })
    }

    fn DoPreview(&self) -> Result<()> {
        guarded(|| {
            let mut state = self.state()?;
            let parent = state.parent.ok_or_else(|| Error::from(E_UNEXPECTED))?;
            // A second `DoPreview` replaces the first preview.
            state.window = None;
            let content = {
                let text = state
                    .text
                    .as_deref()
                    .ok_or_else(|| Error::from(E_UNEXPECTED))?;
                let system = read_system_theme(&Win32Settings);
                let engine = DWriteEngine::new().map_err(to_error)?;
                PreviewContent::new(text, state.kind, &state.visuals, &system, engine)
            };
            let window = PreviewWindow::create(parent, state.rect, content, state.frame())
                .map_err(to_error)?;
            state.window = Some(window);
            Ok(())
        })
    }

    /// Destroys the preview and forgets the file; the handler can be initialized again.
    fn Unload(&self) -> Result<()> {
        guarded(|| {
            let mut state = self.state()?;
            state.window = None;
            state.text = None;
            Ok(())
        })
    }

    fn SetFocus(&self) -> Result<()> {
        guarded(|| {
            if let Some(window) = &self.state()?.window {
                window.focus();
            }
            Ok(())
        })
    }

    fn QueryFocus(&self) -> Result<HWND> {
        guarded(|| {
            // SAFETY: plain query of the calling thread's focus window.
            let focus = unsafe { GetFocus() };
            if focus.is_invalid() {
                Err(Error::from(E_FAIL))
            } else {
                Ok(focus)
            }
        })
    }

    /// Offers a key to the host's frame (the pane's own accelerators).
    fn TranslateAccelerator(&self, pmsg: *const MSG) -> Result<()> {
        guarded(|| {
            let frame = self.state()?.frame();
            match frame {
                // SAFETY: `pmsg` is the message the host passed to this call; it is handed on
                // unchanged.
                Some(frame) => unsafe { frame.TranslateAccelerator(pmsg) },
                None => Err(Error::from(S_FALSE)),
            }
        })
    }
}

#[allow(non_snake_case)]
impl IObjectWithSite_Impl for PreviewHandler_Impl {
    fn SetSite(&self, punksite: Ref<IUnknown>) -> Result<()> {
        guarded(|| {
            let mut state = self.state()?;
            state.site = punksite.cloned();
            let frame = state.frame();
            if let Some(window) = &state.window {
                window.set_frame(frame);
            }
            Ok(())
        })
    }

    fn GetSite(&self, riid: *const GUID, ppvsite: *mut *mut c_void) -> Result<()> {
        guarded(|| {
            if riid.is_null() || ppvsite.is_null() {
                return Err(Error::from(E_POINTER));
            }
            // SAFETY: `ppvsite` is non-null and valid for writing a pointer (the caller's
            // contract); a failure leaves it null.
            unsafe { *ppvsite = std::ptr::null_mut() };
            let state = self.state.try_borrow().map_err(|_| Error::from(E_FAIL))?;
            let site = state.site.as_ref().ok_or_else(|| Error::from(E_FAIL))?;
            // SAFETY: both pointers are non-null; `ppvsite` receives an owned interface pointer.
            unsafe { site.query(riid, ppvsite) }.ok()
        })
    }
}

#[allow(non_snake_case)]
impl IOleWindow_Impl for PreviewHandler_Impl {
    fn GetWindow(&self) -> Result<HWND> {
        guarded(|| {
            let state = self.state.try_borrow().map_err(|_| Error::from(E_FAIL))?;
            state
                .window
                .as_ref()
                .map(PreviewWindow::hwnd)
                .ok_or_else(|| Error::from(E_FAIL))
        })
    }

    fn ContextSensitiveHelp(&self, _fentermode: BOOL) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
}

#[allow(non_snake_case)]
impl IPreviewHandlerVisuals_Impl for PreviewHandler_Impl {
    fn SetBackgroundColor(&self, color: COLORREF) -> Result<()> {
        guarded(|| {
            let mut state = self.state()?;
            state.visuals.set_background(color.0);
            state.apply_visuals();
            Ok(())
        })
    }

    fn SetFont(&self, plf: *const LOGFONTW) -> Result<()> {
        guarded(|| {
            // SAFETY: the host passes a pointer to a valid LOGFONTW (or null, rejected here).
            let font = unsafe { plf.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
            let mut state = self.state()?;
            state.visuals.set_font(font);
            state.apply_visuals();
            Ok(())
        })
    }

    fn SetTextColor(&self, color: COLORREF) -> Result<()> {
        guarded(|| {
            let mut state = self.state()?;
            state.visuals.set_text(color.0);
            state.apply_visuals();
            Ok(())
        })
    }
}

/// The preview handler `handler` points to, when it is this crate's.
fn handler_of(handler: &IPreviewHandler) -> std::result::Result<&PreviewHandler_Impl, WinError> {
    handler
        .cast_object_ref::<PreviewHandler>()
        .map_err(|_| WinError::Invalid("not a preview handler of this crate"))
}

/// Renders the preview `handler` is showing (after `DoPreview`) into a new
/// `width_px` x `height_px` bitmap at `dpi` with the software Direct2D target: the layout,
/// paint and scene passes of the child window, without its swap chain. For tests and tools that
/// check what the pane shows; the window keeps its own layout.
///
/// # Errors
/// Fails when `handler` is not a handler of this crate, no preview is showing or rendering
/// fails.
pub fn render_preview(
    handler: &IPreviewHandler,
    width_px: u32,
    height_px: u32,
    dpi: u32,
) -> std::result::Result<DecodedImage, WinError> {
    let handler = handler_of(handler)?;
    let state = handler
        .state
        .try_borrow()
        .map_err(|_| WinError::Invalid("the preview handler is busy"))?;
    let window = state
        .window
        .as_ref()
        .ok_or(WinError::Invalid("no preview is showing"))?;
    window.render(width_px, height_px, dpi)
}

/// How far the preview of `handler` is scrolled, in DIPs; `None` when no preview is showing or
/// `handler` is not a handler of this crate.
#[must_use]
pub fn scroll_offset(handler: &IPreviewHandler) -> Option<f32> {
    let handler = handler_of(handler).ok()?;
    let state = handler.state.try_borrow().ok()?;
    state.window.as_ref()?.scroll_y()
}

/// A new preview handler, the object a class factory hands out.
pub(crate) fn new_handler() -> IUnknown {
    PreviewHandler::new().into()
}
