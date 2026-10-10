//! The whole preview protocol in process: class factory, `Initialize` with a memory stream,
//! `SetWindow` with a hidden never-shown host, `DoPreview`, rendering the pane to a bitmap and
//! checking its pixels, scrolling, resizing, `Unload`, protocol errors, oversized and failing
//! streams, the frame that receives unused keys.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{
    BLACK, Fault, HEIGHT, RECT_PANE, Session, WHITE, WIDTH, differs, first_ink_column, ink,
    ink_rows, pixel, scripted_stream,
};
use ssw_preview::handler::{render_preview, scroll_offset};
use ssw_preview::notice::{for_error, localizer};
use ssw_preview::source::{MAX_PREVIEW_BYTES, SourceError};
use ssw_win::locale::ui_language;
use windows::Win32::Foundation::{
    E_FAIL, E_INVALIDARG, E_NOTIMPL, E_POINTER, E_UNEXPECTED, HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{ClientToScreen, LOGFONTW};
use windows::Win32::System::Ole::IObjectWithSite;
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{
    IPreviewHandlerFrame, IPreviewHandlerFrame_Impl, IPreviewHandlerVisuals,
    PREVIEWHANDLERFRAMEINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, IsWindow, MSG, SendMessageW, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL,
};
use windows::core::{HRESULT, IUnknown, Interface, implement};

const WHITE_PIXEL: [u8; 4] = [255, 255, 255, 255];

const SAMPLE: &str = "# Title\n\nSome **bold** text and `code`.\n\n- one\n- two\n";

fn long_document() -> String {
    (0..80)
        .map(|i| format!("Paragraph {i} of the long document.\n\n"))
        .collect()
}

fn is_window(hwnd: HWND) -> bool {
    // SAFETY: plain query; stale handles answer false.
    unsafe { IsWindow(Some(hwnd)) }.as_bool()
}

fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rect = RECT::default();
    // SAFETY: valid out pointer for a live window.
    unsafe { GetClientRect(hwnd, &mut rect) }.unwrap();
    (rect.right - rect.left, rect.bottom - rect.top)
}

/// Sends a wheel notch (`notches` > 0 is wheel up) at client point (50, 50).
fn wheel(child: HWND, notches: i16) {
    let mut point = POINT { x: 50, y: 50 };
    // SAFETY: converts a point with a live window.
    assert!(unsafe { ClientToScreen(child, &mut point) }.as_bool());
    let lparam = ((point.y as i16 as u16 as isize) << 16) | (point.x as i16 as u16 as isize);
    let wparam = ((notches * 120) as u16 as usize) << 16;
    // SAFETY: a synchronous message to a window of this thread.
    unsafe {
        SendMessageW(
            child,
            WM_MOUSEWHEEL,
            Some(WPARAM(wparam)),
            Some(LPARAM(lparam)),
        )
    };
}

fn mouse(child: HWND, message: u32, x: i32, y: i32, buttons: usize) {
    let lparam = (y << 16) | (x & 0xFFFF);
    // SAFETY: a synchronous message to a window of this thread.
    unsafe {
        SendMessageW(
            child,
            message,
            Some(WPARAM(buttons)),
            Some(LPARAM(lparam as isize)),
        );
    }
}

#[test]
fn a_document_is_rendered_as_text_on_the_background() {
    let session = Session::show(SAMPLE);
    let image = session.render();
    assert_eq!((image.width, image.height), (WIDTH, HEIGHT));
    assert!(
        !differs(pixel(&image, 2, 2), WHITE_PIXEL, 2),
        "{:?}",
        pixel(&image, 2, 2)
    );
    assert!(ink(&image, WHITE_PIXEL) > 300, "text was drawn");
    let rows = ink_rows(&image, WHITE_PIXEL);
    assert!(
        rows.first().copied().unwrap() < 120,
        "the document starts at the top"
    );
    assert!(
        rows.last().copied().unwrap() < 300,
        "nothing below the last line"
    );
}

#[test]
fn rendering_is_deterministic_and_follows_the_text() {
    let first = Session::show("# One\n").render();
    let again = Session::show("# One\n").render();
    assert_eq!(first.bgra, again.bgra);
    let other = Session::show("# Two\n").render();
    assert_ne!(first.bgra, other.bgra);
}

#[test]
fn an_empty_file_shows_only_the_background() {
    let image = Session::show("").render();
    assert_eq!(ink(&image, WHITE_PIXEL), 0);
}

#[test]
fn markdown_markers_are_hidden() {
    let plain = first_ink_column(&Session::show("bold\n").render(), WHITE_PIXEL).unwrap();
    let strong = first_ink_column(&Session::show("**bold**\n").render(), WHITE_PIXEL).unwrap();
    let heading = first_ink_column(&Session::show("# bold\n").render(), WHITE_PIXEL).unwrap();
    assert!(plain.abs_diff(strong) <= 3, "{plain} vs {strong}");
    assert!(plain.abs_diff(heading) <= 8, "{plain} vs {heading}");
}

#[test]
fn math_and_diagrams_draw_something_other_than_their_source() {
    let good = Session::show("Euler: $e^{i\\pi}+1=0$\n").render();
    let broken = Session::show("Euler: $\\frac{1$\n").render();
    assert!(ink(&good, WHITE_PIXEL) > 100 && ink(&broken, WHITE_PIXEL) > 100);
    assert_ne!(good.bgra, broken.bgra);
    let diagram = Session::show("```mermaid\ngraph TD\n  A[Start] --> B[End]\n```\n").render();
    let code = Session::show("```text\ngraph TD\n  A[Start] --> B[End]\n```\n").render();
    assert!(ink(&diagram, WHITE_PIXEL) > 100);
    assert_ne!(diagram.bgra, code.bgra, "the diagram is not the code block");
}

#[test]
fn the_hosts_colors_paint_the_pane() {
    let session = Session::new();
    session.colors(0x0010_1010, 0x00F0_F0F0);
    session.initialize(SAMPLE.as_bytes()).unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    let image = session.render();
    let dark = pixel(&image, 2, 2);
    assert!(!differs(dark, [16, 16, 16, 255], 3), "{dark:?}");
    let brightest = (0..HEIGHT)
        .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
        .map(|(x, y)| pixel(&image, x, y)[1])
        .max()
        .unwrap();
    assert!(brightest > 180, "light text on the dark pane: {brightest}");
}

#[test]
fn colors_set_after_the_preview_started_repaint_it() {
    let session = Session::show(SAMPLE);
    let before = session.render();
    session.colors(0x00A0_0000, BLACK);
    let after = session.render();
    assert_ne!(before.bgra, after.bgra);
    // COLORREF is 0x00BBGGRR; the bitmap is BGRA.
    assert!(
        !differs(pixel(&after, 2, 2), [0xA0, 0, 0, 255], 3),
        "{:?}",
        pixel(&after, 2, 2)
    );
}

#[test]
fn the_hosts_font_changes_the_text() {
    let default = Session::show(SAMPLE).render();
    let session = Session::new();
    session.colors(WHITE, BLACK);
    let mut font = LOGFONTW::default();
    for (slot, unit) in font.lfFaceName.iter_mut().zip("Consolas".encode_utf16()) {
        *slot = unit;
    }
    let visuals: IPreviewHandlerVisuals = session.handler.cast().unwrap();
    // SAFETY: a valid LOGFONTW.
    unsafe { visuals.SetFont(&font) }.unwrap();
    session.initialize(SAMPLE.as_bytes()).unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    assert_ne!(session.render().bgra, default.bgra);
}

#[test]
fn the_wheel_scrolls_the_document_and_stops_at_the_top() {
    let session = Session::show(&long_document());
    let child = session.child().unwrap();
    assert_eq!(scroll_offset(&session.handler), Some(0.0));
    let top = session.render();
    wheel(child, -1);
    let down = scroll_offset(&session.handler).unwrap();
    assert!(down > 0.0, "{down}");
    assert_ne!(session.render().bgra, top.bgra, "the page moved");
    wheel(child, 1);
    assert!(scroll_offset(&session.handler).unwrap().abs() < 0.5);
    wheel(child, 1);
    assert_eq!(
        scroll_offset(&session.handler),
        Some(0.0),
        "no scrolling above the top"
    );
}

#[test]
fn dragging_with_the_mouse_selects_text() {
    let session = Session::show("Hello selected world, a line of text.\n");
    let child = session.child().unwrap();
    let before = session.render();
    mouse(child, WM_LBUTTONDOWN, 30, 40, 1);
    mouse(child, WM_MOUSEMOVE, 400, 40, 1);
    mouse(child, WM_LBUTTONUP, 400, 40, 0);
    assert_ne!(
        session.render().bgra,
        before.bgra,
        "the selection is painted"
    );
    assert!(is_window(child));
}

#[test]
fn set_rect_resizes_the_child_window() {
    let session = Session::show(SAMPLE);
    let child = session.child().unwrap();
    assert_eq!(client_size(child), (WIDTH as i32, HEIGHT as i32));
    let smaller = RECT {
        left: 0,
        top: 0,
        right: 300,
        bottom: 200,
    };
    // SAFETY: a valid rectangle.
    unsafe { session.handler.SetRect(&smaller) }.unwrap();
    assert_eq!(client_size(child), (300, 200));
    assert!(
        ink(&session.render(), WHITE_PIXEL) > 100,
        "still renders after the resize"
    );
}

#[test]
fn unload_destroys_the_window_and_the_handler_can_start_over() {
    let session = Session::show(SAMPLE);
    let child = session.child().unwrap();
    assert!(is_window(child));
    // A second DoPreview replaces the first window.
    session.do_preview().unwrap();
    assert!(!is_window(child));
    let second = session.child().unwrap();
    assert!(is_window(second));
    // SAFETY: plain call.
    unsafe { session.handler.Unload() }.unwrap();
    assert!(!is_window(second));
    assert!(session.child().is_err());
    assert!(render_preview(&session.handler, 100, 100, 96).is_err());
    assert_eq!(scroll_offset(&session.handler), None);
    assert_eq!(
        session.do_preview().unwrap_err().code(),
        E_UNEXPECTED,
        "the file is forgotten"
    );
    session.initialize(b"# Again\n").unwrap();
    session.do_preview().unwrap();
    assert!(is_window(session.child().unwrap()));
}

#[test]
fn the_protocol_is_enforced_with_error_codes() {
    let session = Session::new();
    assert_eq!(
        session.do_preview().unwrap_err().code(),
        E_UNEXPECTED,
        "no file yet"
    );
    session.initialize(b"x").unwrap();
    assert_eq!(
        session.do_preview().unwrap_err().code(),
        E_UNEXPECTED,
        "no window yet"
    );
    let again = session.initialize(b"y").unwrap_err();
    assert_eq!(
        again.code(),
        HRESULT::from_win32(1247),
        "already initialized"
    );
    let init: IInitializeWithStream = session.handler.cast().unwrap();
    // SAFETY: a null stream is the point.
    let null = unsafe { init.Initialize(None::<&windows::Win32::System::Com::IStream>, 0) };
    assert_eq!(null.unwrap_err().code(), E_POINTER);
    // SAFETY: a null window and a null rectangle are the point.
    unsafe {
        assert_eq!(
            session
                .handler
                .SetWindow(HWND::default(), &RECT_PANE)
                .unwrap_err()
                .code(),
            E_INVALIDARG
        );
        assert_eq!(
            session
                .handler
                .SetWindow(session.parent.0, std::ptr::null())
                .unwrap_err()
                .code(),
            E_POINTER
        );
    }
}

#[test]
fn an_oversized_file_shows_the_notice_instead() {
    let notice = for_error(
        &localizer(&ui_language()),
        &SourceError::TooLarge {
            limit: MAX_PREVIEW_BYTES,
        },
    );
    let expected = Session::show(&notice).render();
    let session = Session::new();
    session.colors(WHITE, BLACK);
    session
        .initialize(&vec![b'a'; MAX_PREVIEW_BYTES + 1])
        .unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    let image = session.render();
    assert!(ink(&image, WHITE_PIXEL) > 300);
    assert_eq!(image.bgra, expected.bgra);
}

#[test]
fn a_stream_that_never_ends_is_cut_off_like_an_oversized_file() {
    let notice = for_error(
        &localizer(&ui_language()),
        &SourceError::TooLarge {
            limit: MAX_PREVIEW_BYTES,
        },
    );
    let expected = Session::show(&notice).render();
    let session = Session::new();
    session.colors(WHITE, BLACK);
    session
        .initialize_with(&scripted_stream(Fault::Endless, &[]))
        .unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    assert_eq!(session.render().bgra, expected.bgra);
}

#[test]
fn a_failing_stream_shows_the_unreadable_notice() {
    let read_error = SourceError::Read(windows::core::Error::from(E_FAIL));
    let expected = Session::show(&for_error(&localizer(&ui_language()), &read_error)).render();
    let session = Session::new();
    session.colors(WHITE, BLACK);
    session
        .initialize_with(&scripted_stream(Fault::FailRead, &[]))
        .unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    let image = session.render();
    assert!(ink(&image, WHITE_PIXEL) > 100);
    assert_eq!(image.bgra, expected.bgra);
}

/// A host frame that records the keys it is offered.
#[implement(IPreviewHandlerFrame)]
struct Frame {
    keys: Rc<RefCell<Vec<(u32, usize)>>>,
}

#[allow(non_snake_case)]
impl IPreviewHandlerFrame_Impl for Frame_Impl {
    fn GetWindowContext(&self) -> windows::core::Result<PREVIEWHANDLERFRAMEINFO> {
        Err(E_NOTIMPL.into())
    }

    fn TranslateAccelerator(&self, pmsg: *const MSG) -> windows::core::Result<()> {
        // SAFETY: the caller passes a valid message.
        let message = unsafe { *pmsg };
        self.keys
            .borrow_mut()
            .push((message.message, message.wParam.0));
        Ok(())
    }
}

use windows::Win32::System::Ole::IOleWindow;
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandler_Impl};

const FLOW: &str = "graph TD\n  A[Start] --> B[End]\n";

/// The pane of a stream that reports `name` and holds [`FLOW`].
fn render_named_stream(name: &'static str) -> ssw_win::wic::DecodedImage {
    let session = Session::new();
    session.colors(WHITE, BLACK);
    session
        .initialize_with(&scripted_stream(Fault::Named(name), FLOW.as_bytes()))
        .unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    session.render()
}

#[test]
fn a_stream_named_mmd_is_one_diagram_and_other_names_are_markdown() {
    let diagram = render_named_stream("flow.mmd");
    let markdown = render_named_stream("flow.md");
    assert!(ink(&diagram, WHITE_PIXEL) > 100);
    assert_ne!(
        diagram.bgra, markdown.bgra,
        "the diagram is drawn, not its source"
    );
    assert_eq!(
        markdown.bgra,
        Session::show(FLOW).render().bgra,
        "any other name is a Markdown file"
    );
}

#[test]
fn the_pane_rectangle_is_kept_until_the_preview_starts_and_followed_afterwards() {
    let session = Session::new();
    session.colors(WHITE, BLACK);
    session.initialize(SAMPLE.as_bytes()).unwrap();
    session.set_window().unwrap();
    let smaller = RECT {
        left: 10,
        top: 20,
        right: 310,
        bottom: 220,
    };
    // SAFETY: a valid rectangle.
    unsafe { session.handler.SetRect(&smaller) }.unwrap();
    session.do_preview().unwrap();
    let child = session.child().unwrap();
    assert_eq!(
        client_size(child),
        (300, 200),
        "DoPreview uses the latest rectangle"
    );
    // Another host takes the preview over with its own rectangle.
    let other = common::Parent::new();
    let moved = RECT {
        left: 0,
        top: 0,
        right: 200,
        bottom: 100,
    };
    // SAFETY: valid window handle and rectangle.
    unsafe { session.handler.SetWindow(other.0, &moved) }.unwrap();
    // SAFETY: a plain query of the parent of a live window.
    let parent = unsafe { windows::Win32::UI::WindowsAndMessaging::GetParent(child) };
    assert_eq!(parent.unwrap(), other.0);
    assert_eq!(client_size(child), (200, 100));
    // SAFETY: valid window handle and rectangle.
    unsafe { session.handler.SetWindow(session.parent.0, &RECT_PANE) }.unwrap();
}

#[test]
fn a_host_window_that_is_gone_fails_the_preview_and_the_handler_recovers() {
    let session = Session::new();
    session.initialize(b"x").unwrap();
    let gone = common::Parent::new().0;
    // SAFETY: a stale but non-null window handle is the point.
    unsafe { session.handler.SetWindow(gone, &RECT_PANE) }.unwrap();
    assert_eq!(
        session.do_preview().unwrap_err().code(),
        HRESULT::from_win32(1400),
        "ERROR_INVALID_WINDOW_HANDLE"
    );
    assert!(session.child().is_err());
    session.set_window().unwrap();
    session.do_preview().unwrap();
    assert!(is_window(session.child().unwrap()));
}

#[test]
fn the_focus_goes_to_the_pane_and_the_keys_it_uses_stay_out_of_the_hosts_frame() {
    let session = Session::new();
    let keys = Rc::new(RefCell::new(Vec::new()));
    let frame: IPreviewHandlerFrame = Frame {
        keys: Rc::clone(&keys),
    }
    .into();
    let site: IUnknown = frame.cast().unwrap();
    let with_site: IObjectWithSite = session.handler.cast().unwrap();
    // SAFETY: plain call with live interface pointers.
    unsafe { with_site.SetSite(&site) }.unwrap();
    session.colors(WHITE, BLACK);
    session.initialize(SAMPLE.as_bytes()).unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    let child = session.child().unwrap();
    // SAFETY: plain call.
    let nobody = unsafe { session.handler.QueryFocus() };
    assert_eq!(
        nobody.unwrap_err().code(),
        E_FAIL,
        "nothing has the focus yet"
    );
    let down = || {
        // SAFETY: a synchronous message to a window of this thread.
        unsafe { SendMessageW(child, WM_KEYDOWN, Some(WPARAM(0x28)), Some(LPARAM(0))) };
    };
    down();
    assert_eq!(
        *keys.borrow(),
        [(WM_KEYDOWN, 0x28)],
        "an unfocused pane passes the key on"
    );
    // SAFETY: plain calls.
    unsafe { session.handler.SetFocus() }.unwrap();
    // SAFETY: plain call.
    assert_eq!(unsafe { session.handler.QueryFocus() }.unwrap(), child);
    down();
    assert_eq!(
        keys.borrow().len(),
        1,
        "a focused pane keeps the arrow keys"
    );
    // A click on the focused pane does not ask for the focus again.
    mouse(child, WM_LBUTTONDOWN, 30, 40, 1);
    mouse(child, WM_LBUTTONUP, 30, 40, 0);
    assert_eq!(keys.borrow().len(), 1);
}

/// A host site that calls back into the handler while the handler releases it.
#[implement(IPreviewHandlerFrame)]
struct BusySite {
    on_release: RefCell<Option<Box<dyn FnOnce()>>>,
}

impl Drop for BusySite {
    fn drop(&mut self) {
        if let Some(call) = self.on_release.get_mut().take() {
            call();
        }
    }
}

#[allow(non_snake_case)]
impl IPreviewHandlerFrame_Impl for BusySite_Impl {
    fn GetWindowContext(&self) -> windows::core::Result<PREVIEWHANDLERFRAMEINFO> {
        Err(E_NOTIMPL.into())
    }

    fn TranslateAccelerator(&self, _pmsg: *const MSG) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
}

#[test]
fn calls_that_arrive_while_the_handler_is_busy_fail_instead_of_aliasing_its_state() {
    let session = Session::new();
    let codes = Rc::new(RefCell::new(Vec::new()));
    let refused_inspection = Rc::new(RefCell::new(false));
    let (log, inspected) = (Rc::clone(&codes), Rc::clone(&refused_inspection));
    let handler = session.handler.clone();
    let parent = session.parent.0;
    let call_back = move || {
        let init: IInitializeWithStream = handler.cast().unwrap();
        let with_site: IObjectWithSite = handler.cast().unwrap();
        let ole: IOleWindow = handler.cast().unwrap();
        let visuals: IPreviewHandlerVisuals = handler.cast().unwrap();
        let mut seen = log.borrow_mut();
        let mut note = |result: windows::core::Result<()>| seen.push(result.unwrap_err().code());
        // SAFETY: plain calls with live interface pointers and valid arguments; every one of
        // them is refused because the handler is in the middle of `SetSite`.
        unsafe {
            note(init.Initialize(&common::memory_stream(b"x"), 0));
            note(handler.SetWindow(parent, &RECT_PANE));
            note(handler.SetRect(&RECT_PANE));
            note(handler.DoPreview());
            note(handler.Unload());
            note(handler.SetFocus());
            note(handler.TranslateAccelerator(&MSG::default()));
            note(with_site.SetSite(None::<&IUnknown>));
            note(with_site.GetSite::<IUnknown>().map(|_| ()));
            note(ole.GetWindow().map(|_| ()));
            note(visuals.SetBackgroundColor(windows::Win32::Foundation::COLORREF(0)));
            note(visuals.SetFont(&LOGFONTW::default()));
            note(visuals.SetTextColor(windows::Win32::Foundation::COLORREF(0)));
        }
        *inspected.borrow_mut() =
            render_preview(&handler, 10, 10, 96).is_err() && scroll_offset(&handler).is_none();
    };
    let frame: IPreviewHandlerFrame = BusySite {
        on_release: RefCell::new(Some(Box::new(call_back))),
    }
    .into();
    let site: IUnknown = frame.cast().unwrap();
    let with_site: IObjectWithSite = session.handler.cast().unwrap();
    // SAFETY: plain calls with live interface pointers; dropping the host's own references
    // leaves the handler holding the only one, which `SetSite(None)` releases inside the call.
    unsafe { with_site.SetSite(&site) }.unwrap();
    drop((site, frame));
    // SAFETY: as above.
    unsafe { with_site.SetSite(None::<&IUnknown>) }.unwrap();
    assert_eq!(*codes.borrow(), vec![E_FAIL; 13]);
    assert!(
        *refused_inspection.borrow(),
        "inspection refuses a busy handler too"
    );
}

/// A preview handler of some other implementation.
#[implement(IPreviewHandler)]
struct Stranger;

#[allow(non_snake_case)]
impl IPreviewHandler_Impl for Stranger_Impl {
    fn SetWindow(&self, _hwnd: HWND, _prc: *const RECT) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetRect(&self, _prc: *const RECT) -> windows::core::Result<()> {
        Ok(())
    }

    fn DoPreview(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Unload(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn SetFocus(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn QueryFocus(&self) -> windows::core::Result<HWND> {
        Ok(HWND::default())
    }

    fn TranslateAccelerator(&self, _pmsg: *const MSG) -> windows::core::Result<()> {
        Ok(())
    }
}

#[test]
fn the_inspection_helpers_refuse_handlers_of_other_implementations() {
    let stranger: IPreviewHandler = Stranger.into();
    let refused = render_preview(&stranger, 10, 10, 96).err();
    assert!(
        matches!(
            refused,
            Some(ssw_win::com::WinError::Invalid(
                "not a preview handler of this crate"
            ))
        ),
        "{refused:?}"
    );
    assert_eq!(scroll_offset(&stranger), None);
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_platform_failure_while_previewing_or_rendering_is_a_clean_error() {
    use ssw_preview::fault;

    for n in 0.. {
        let session = Session::new();
        session.colors(WHITE, BLACK);
        session.initialize(b"Some $x^2$ text\n").unwrap();
        session.set_window().unwrap();
        fault::arm(n);
        let result = session.do_preview();
        let tripped = fault::tripped();
        fault::disarm();
        if !tripped {
            result.unwrap();
            break;
        }
        assert_eq!(result.unwrap_err().code(), E_FAIL, "check {n}");
        assert!(
            session.child().is_err(),
            "check {n}: nothing is left showing"
        );
        session.do_preview().unwrap();
        assert!(
            is_window(session.child().unwrap()),
            "check {n}: the handler recovers"
        );
    }
    let session = Session::show(SAMPLE);
    for n in 0.. {
        fault::arm(n);
        let image = render_preview(&session.handler, WIDTH, HEIGHT, 96);
        let tripped = fault::tripped();
        fault::disarm();
        if !tripped {
            assert!(image.is_ok());
            break;
        }
        let error = image.err().unwrap();
        assert!(
            matches!(error, ssw_win::com::WinError::Invalid("injected fault")),
            "check {n}: {error:?}"
        );
        let size = client_size(session.child().unwrap());
        assert_eq!(
            size,
            (WIDTH as i32, HEIGHT as i32),
            "check {n}: the window is untouched"
        );
    }
}

#[test]
fn keys_the_preview_does_not_use_go_to_the_hosts_frame() {
    let session = Session::new();
    let keys = Rc::new(RefCell::new(Vec::new()));
    let frame: IPreviewHandlerFrame = Frame {
        keys: Rc::clone(&keys),
    }
    .into();
    let site: IUnknown = frame.cast().unwrap();
    let with_site: IObjectWithSite = session.handler.cast().unwrap();
    // SAFETY: plain call with live interface pointers.
    unsafe { with_site.SetSite(&site) }.unwrap();
    // SAFETY: plain call.
    assert!(unsafe { with_site.GetSite::<IPreviewHandlerFrame>() }.is_ok());
    session.colors(WHITE, BLACK);
    session.initialize(SAMPLE.as_bytes()).unwrap();
    session.set_window().unwrap();
    session.do_preview().unwrap();
    let child = session.child().unwrap();
    // SAFETY: a synchronous message to a window of this thread.
    unsafe { SendMessageW(child, WM_KEYDOWN, Some(WPARAM(0x09)), Some(LPARAM(0))) };
    assert_eq!(
        *keys.borrow(),
        [(WM_KEYDOWN, 0x09)],
        "Tab belongs to the host"
    );
    // The host can also hand a key to the handler, which offers it to the frame.
    let message = MSG {
        message: WM_KEYDOWN,
        wParam: WPARAM(0x70),
        ..MSG::default()
    };
    // SAFETY: a valid message.
    unsafe { session.handler.TranslateAccelerator(&message) }.unwrap();
    assert_eq!(keys.borrow().len(), 2);
    // Without a site nothing is forwarded.
    // SAFETY: plain call.
    unsafe { with_site.SetSite(None::<&IUnknown>) }.unwrap();
    // SAFETY: a synchronous message to a window of this thread.
    unsafe { SendMessageW(child, WM_KEYDOWN, Some(WPARAM(0x09)), Some(LPARAM(0))) };
    assert_eq!(keys.borrow().len(), 2);
}
