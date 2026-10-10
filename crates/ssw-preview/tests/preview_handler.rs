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
