//! DirectManipulation on an off-screen window that is never activated or focused, exactly the
//! fixture of the spike that worked: `WS_OVERLAPPEDWINDOW` with
//! `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` (no taskbar button) at (-20000, -20000), shown with
//! `SW_SHOWNOACTIVATE`. A window that was never shown receives no DirectManipulation updates,
//! and DirectManipulation (an STA object) needs the window's thread to pump messages: every
//! wait loop here dispatches the thread's messages like the real message loop does. The tests
//! share one process, so a process-wide lock serializes them: managers must not overlap.
//! No pointer is injected: the viewport event handler is driven through the
//! viewport's own `ZoomToRect` (immediate and animated), and `SetContact` is exercised with a
//! pointer id the system does not know. Real touch and touchpad contacts are manual checks.

mod support;

use std::time::{Duration, Instant};

use support::MachineLock;

use ssw_ui::geometry::Size;
use ssw_ui::input::{Modifiers, WheelEvent};
use ssw_ui::scroll_physics::{Extent, WheelLines};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dmanip::{
    DirectManipulation, DmConfig, DmListener, DmStatus, DmTransform, ScrollDriver, ScrollFrame,
    ScrollLayout, ZoomRange, client_area_animation, pointer_client_position, pointer_id,
    pointer_is_pen, wants_contact, wheel_scroll_lines,
};
use windows::Win32::Foundation::{ERROR_NOT_FOUND, HINSTANCE, HWND, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DM_POINTERHITTEST, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
    POINTER_INPUT_TYPE, PT_MOUSE, PT_PEN, PT_TOUCH, PT_TOUCHPAD, PeekMessageW, SW_SHOWNOACTIVATE,
    ShowWindow, TranslateMessage, WM_POINTERDOWN, WM_POINTERUP, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HRESULT, w};

/// How long a test waits for DirectManipulation to deliver something.
const PATIENCE: Duration = Duration::from_secs(8);
const FRAME: Duration = Duration::from_millis(8);

/// A fully transparent, click-through, off-screen window of the system `STATIC` class that is
/// shown without activation. The second field holds the machine-wide DirectManipulation lock
/// (one such window at a time, across the processes of a parallel test run) for as long as the
/// window lives.
struct HiddenWindow(HWND, #[allow(dead_code)] MachineLock);

/// Dispatches the messages queued for this thread, as the message loop of the real window does
/// between frames: DirectManipulation (an STA object) needs the thread to pump to deliver.
fn pump_messages() {
    let mut message = MSG::default();
    // SAFETY: the standard non-blocking message pump of the thread that owns the window.
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

impl HiddenWindow {
    fn new() -> Self {
        let guard = MachineLock::acquire("directmanipulation");
        // SAFETY: querying the handle of the current module.
        let instance = unsafe { GetModuleHandleW(None) }
            .map(|module| HINSTANCE(module.0))
            .unwrap_or_default();
        // SAFETY: creates a window of a system class, far off every monitor, never activated.
        let window = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPEDWINDOW,
                -20000,
                -20000,
                916,
                739,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("the hidden window is created");
        // SAFETY: the window was just created on this thread; `SW_SHOWNOACTIVATE` shows it
        // without taking focus.
        unsafe {
            let _ = ShowWindow(window, SW_SHOWNOACTIVATE);
        }
        pump_messages();
        HiddenWindow(window, guard)
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        // SAFETY: destroys the window this value created, on the thread that created it.
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

/// Everything a viewport reported, in order.
#[derive(Default)]
struct Recorder {
    scrolls: Vec<(f64, f64)>,
    zooms: Vec<f64>,
    statuses: Vec<(DmStatus, DmStatus)>,
    /// `"z"` for a zoom, `"s"` for a scroll, in delivery order.
    order: Vec<&'static str>,
}

impl DmListener for Recorder {
    fn scrolled(&mut self, offset_x: f64, offset_y: f64) {
        self.scrolls.push((offset_x, offset_y));
        self.order.push("s");
    }

    fn zoomed(&mut self, zoom: f64) {
        self.zooms.push(zoom);
        self.order.push("z");
    }

    fn status_changed(&mut self, current: DmStatus, previous: DmStatus) {
        self.statuses.push((current, previous));
    }
}

fn approx(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} is not within {tolerance} of {expected}"
    );
}

/// The last reported scroll offset is `y` (to a hundredth of a DIP): immediate moves land on it
/// exactly.
fn scrolled_to(recorder: &Recorder, y: f64) -> bool {
    scrolled_near(recorder, y, 0.01)
}

/// The last reported scroll offset is within `tolerance` DIPs of `y`.
fn scrolled_near(recorder: &Recorder, y: f64, tolerance: f64) -> bool {
    recorder
        .scrolls
        .last()
        .is_some_and(|&(_, last)| (last - y).abs() < tolerance)
}

/// How close DirectManipulation's own animation ends to its target: it settles a few hundredths
/// of a DIP short (observed 799.957 for 800, 299.98 for 300), so animated moves are compared to
/// half a pixel and recognized by their end (`Ready`), never by an exact value.
const ANIMATION_SLACK: f64 = 0.5;

fn viewport() -> Size {
    Size::new(400.0, 300.0)
}

fn rig(window: &HiddenWindow, config: DmConfig, scale: f32) -> DirectManipulation {
    DirectManipulation::new(
        window.0,
        config,
        viewport(),
        Size::new(400.0, 3000.0),
        scale,
    )
    .expect("the viewport is created")
}

/// Updates the viewport every frame until `done` accepts what was recorded.
fn pump(
    dm: &mut DirectManipulation,
    recorder: &mut Recorder,
    what: &str,
    done: impl Fn(&Recorder) -> bool,
) {
    let start = Instant::now();
    while !done(recorder) {
        assert!(
            start.elapsed() < PATIENCE,
            "no {what} after {PATIENCE:?}: {:?}",
            recorder.scrolls
        );
        pump_messages();
        dm.update(recorder).expect("update");
        std::thread::sleep(FRAME);
    }
}

fn layout(content: f64) -> ScrollLayout {
    ScrollLayout {
        viewport: viewport(),
        extent: Extent::new(300.0, content),
        scale: 1.0,
        line_height: 20.0,
    }
}

fn wheel(y: f32, precise: bool) -> WheelEvent {
    WheelEvent {
        delta: ssw_ui::geometry::Point::new(0.0, y),
        precise,
        position: ssw_ui::geometry::Point::ZERO,
        mods: Modifiers::NONE,
    }
}

fn notch() -> f64 {
    wheel_scroll_lines().notch_distance(20.0, 300.0)
}

/// Runs driver frames on the wall clock until `done` accepts a frame; returns every frame.
fn run_frames(
    driver: &mut ScrollDriver,
    what: &str,
    mut done: impl FnMut(&ScrollFrame) -> bool,
) -> Vec<ScrollFrame> {
    let start = Instant::now();
    let mut frames = Vec::new();
    loop {
        assert!(
            start.elapsed() < PATIENCE,
            "no {what} after {PATIENCE:?}: {:?}",
            frames.last()
        );
        pump_messages();
        std::thread::sleep(FRAME);
        let frame = driver.frame(start.elapsed()).expect("frame");
        let finished = done(&frame);
        frames.push(frame);
        if finished {
            return frames;
        }
    }
}

fn is_non_decreasing(frames: &[ScrollFrame]) -> bool {
    frames
        .windows(2)
        .all(|pair| pair[1].offset >= pair[0].offset)
}

#[test]
fn statuses_map_and_only_manipulation_moves() {
    use windows::Win32::Graphics::DirectManipulation::{
        DIRECTMANIPULATION_BUILDING, DIRECTMANIPULATION_DISABLED, DIRECTMANIPULATION_ENABLED,
        DIRECTMANIPULATION_INERTIA, DIRECTMANIPULATION_READY, DIRECTMANIPULATION_RUNNING,
        DIRECTMANIPULATION_STATUS, DIRECTMANIPULATION_SUSPENDED,
    };
    let table = [
        (DIRECTMANIPULATION_BUILDING, DmStatus::Building, false),
        (DIRECTMANIPULATION_ENABLED, DmStatus::Enabled, false),
        (DIRECTMANIPULATION_DISABLED, DmStatus::Disabled, false),
        (DIRECTMANIPULATION_RUNNING, DmStatus::Running, true),
        (DIRECTMANIPULATION_INERTIA, DmStatus::Inertia, true),
        (DIRECTMANIPULATION_READY, DmStatus::Ready, false),
        (DIRECTMANIPULATION_SUSPENDED, DmStatus::Suspended, false),
        (DIRECTMANIPULATION_STATUS(42), DmStatus::Unknown(42), false),
    ];
    for (raw, status, moving) in table {
        assert_eq!(DmStatus::from_raw(raw), status);
        assert_eq!(status.is_moving(), moving, "{status:?}");
    }
}

#[test]
fn transforms_convert_pixels_to_dips() {
    // Content moved up 300 px and left 10 px at 2 px per DIP, scaled 2x: the visible corner is
    // 150 DIPs down and 5 DIPs right.
    let zoomed = DmTransform::from_matrix(&[2.0, 0.0, 0.0, 2.0, -10.0, -300.0], 2.0);
    assert_eq!(
        zoomed,
        DmTransform {
            zoom: 2.0,
            offset_x: 5.0,
            offset_y: 150.0
        }
    );
    let rest = DmTransform::from_matrix(&[1.0, 0.0, 0.0, 1.0, 0.0, 0.0], 1.5);
    assert_eq!(rest.zoom, 1.0);
    assert_eq!(rest.offset_y, 0.0);
    // Overscroll (content pulled down) is a negative offset.
    let pulled = DmTransform::from_matrix(&[1.0, 0.0, 0.0, 1.0, 0.0, 30.0], 1.0);
    assert_eq!(pulled.offset_y, -30.0);
}

#[test]
fn touch_and_pen_start_on_pointer_down_and_touchpads_on_the_hit_test() {
    for kind in [PT_TOUCH, PT_PEN] {
        assert!(wants_contact(WM_POINTERDOWN, kind));
        assert!(!wants_contact(DM_POINTERHITTEST, kind));
    }
    assert!(wants_contact(DM_POINTERHITTEST, PT_TOUCHPAD));
    assert!(!wants_contact(WM_POINTERDOWN, PT_TOUCHPAD));
    // The mouse never manipulates; other messages never start a contact.
    assert!(!wants_contact(WM_POINTERDOWN, PT_MOUSE));
    assert!(!wants_contact(DM_POINTERHITTEST, PT_MOUSE));
    assert!(!wants_contact(WM_POINTERUP, PT_TOUCH));
    assert!(!wants_contact(WM_POINTERDOWN, POINTER_INPUT_TYPE(0)));
    // The id is the low word of wParam.
    assert_eq!(pointer_id(WPARAM(0x0001_0042)), 0x42);
    assert_eq!(pointer_id(WPARAM(7)), 7);
}

#[test]
fn system_settings_are_read() {
    assert_eq!(client_area_animation(), client_area_animation());
    // Three lines by default, or a page; either way a notch scrolls something.
    assert!(wheel_scroll_lines().notch_distance(20.0, 600.0) > 0.0);
}

#[test]
fn layout_content_size_is_a_viewport_plus_the_scroll_range() {
    assert_eq!(layout(3000.0).content_size(), Size::new(400.0, 3000.0));
    // Content shorter than the viewport: nothing to scroll, the viewport itself.
    assert_eq!(layout(100.0).content_size(), Size::new(400.0, 300.0));
    // Typewriter mode adds half a viewport past the end: 2700 + 150 + 300.
    let typewriter = ScrollLayout {
        extent: Extent::typewriter(300.0, 3000.0, 0.5),
        ..layout(3000.0)
    };
    assert_eq!(typewriter.content_size(), Size::new(400.0, 3150.0));
}

#[test]
fn the_viewport_follows_the_geometry_in_pixels() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut dm = rig(&window, DmConfig::default(), 1.5);
    let rect = |right, bottom| RECT {
        left: 0,
        top: 0,
        right,
        bottom,
    };
    assert_eq!(dm.viewport_rect().unwrap(), rect(600, 450));
    assert_eq!(dm.content_rect().unwrap(), rect(600, 4500));
    assert!(!dm.status().unwrap().is_moving());

    dm.set_geometry(Size::new(500.0, 200.0), Size::new(500.0, 1000.0), 1.0)
        .unwrap();
    assert_eq!(dm.viewport_rect().unwrap(), rect(500, 200));
    assert_eq!(dm.content_rect().unwrap(), rect(500, 1000));
}

#[test]
fn invalid_requests_are_rejected_before_they_reach_the_viewport() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    for (min, max) in [(2.0, 1.0), (0.0, 1.0), (-1.0, 1.0)] {
        let config = DmConfig {
            zoom: Some(ZoomRange { min, max }),
        };
        let error =
            DirectManipulation::new(window.0, config, viewport(), Size::new(400.0, 3000.0), 1.0)
                .unwrap_err();
        assert!(matches!(error, WinError::Invalid(_)), "{error:?}");
    }
    let dm = rig(&window, DmConfig::default(), 1.0);
    for zoom in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(dm.sync_to(100.0, zoom), Err(WinError::Invalid(_))));
        assert!(matches!(
            dm.animate_to(100.0, zoom),
            Err(WinError::Invalid(_))
        ));
    }
}

#[test]
fn an_immediate_move_reaches_the_listener_as_a_scroll_offset() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut dm = rig(&window, DmConfig::default(), 1.0);
    let mut recorder = Recorder::default();

    dm.sync_to(500.0, 1.0).unwrap();
    pump(&mut dm, &mut recorder, "scroll to 500", |recorder| {
        scrolled_to(recorder, 500.0)
    });
    let &(x, y) = recorder.scrolls.last().unwrap();
    approx(x, 0.0, 0.01);
    approx(y, 500.0, 0.01);
    // No zoom was asked for.
    assert!(recorder.zooms.is_empty());
    let transform = dm.content_transform().unwrap();
    approx(transform.zoom, 1.0, 1e-6);
    approx(transform.offset_y, 500.0, 0.01);

    // Back up: the offset follows in both directions.
    dm.sync_to(120.0, 1.0).unwrap();
    pump(&mut dm, &mut recorder, "scroll to 120", |recorder| {
        scrolled_to(recorder, 120.0)
    });
    approx(dm.content_transform().unwrap().offset_y, 120.0, 0.01);
}

#[test]
fn offsets_are_reported_in_dips_at_any_scale() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut dm = rig(&window, DmConfig::default(), 2.0);
    let mut recorder = Recorder::default();
    // 100 DIPs are 200 pixels: the content transform holds pixels, the listener DIPs.
    dm.sync_to(100.0, 1.0).unwrap();
    pump(&mut dm, &mut recorder, "scroll to 100", |recorder| {
        scrolled_to(recorder, 100.0)
    });
    approx(dm.content_transform().unwrap().offset_y, 100.0, 0.01);
}

#[test]
fn an_animated_move_streams_offsets_and_status_changes() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut dm = rig(&window, DmConfig::default(), 1.0);
    let mut recorder = Recorder::default();

    dm.animate_to(800.0, 1.0).unwrap();
    pump(&mut dm, &mut recorder, "animation end", |recorder| {
        scrolled_near(recorder, 800.0, ANIMATION_SLACK)
            && recorder
                .statuses
                .last()
                .is_some_and(|&(current, _)| !current.is_moving())
    });
    // It came in steps, monotonically, and ran as inertia (DirectManipulation's animation).
    let ys: Vec<f64> = recorder.scrolls.iter().map(|&(_, y)| y).collect();
    assert!(ys.len() > 5, "{ys:?}");
    assert!(ys.windows(2).all(|pair| pair[1] >= pair[0]), "{ys:?}");
    assert!(
        recorder
            .statuses
            .iter()
            .any(|&(current, _)| current == DmStatus::Inertia)
    );
    // `stop` on an idle viewport is fine.
    dm.stop().unwrap();
}

#[test]
fn zoom_is_reported_before_the_scroll_of_the_same_update() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let config = DmConfig {
        zoom: Some(ZoomRange { min: 0.5, max: 4.0 }),
    };
    let mut dm = rig(&window, config, 1.0);
    let mut recorder = Recorder::default();

    dm.sync_to(300.0, 2.0).unwrap();
    pump(&mut dm, &mut recorder, "zoom to 2", |recorder| {
        !recorder.zooms.is_empty() && !recorder.scrolls.is_empty()
    });
    approx(recorder.zooms[0], 2.0, 1e-3);
    // The scroll of the update that changed the zoom comes right after the zoom.
    let zoom_at = recorder.order.iter().position(|&kind| kind == "z").unwrap();
    assert_eq!(recorder.order[zoom_at + 1], "s", "{:?}", recorder.order);
    let (_, y) = *recorder.scrolls.last().unwrap();
    approx(y, 300.0, 0.01);
    let transform = dm.content_transform().unwrap();
    approx(transform.zoom, 2.0, 1e-3);
    approx(transform.offset_y, 300.0, 0.01);

    // Back to 1: the zoom is reported again, only when it changes.
    let reported = recorder.zooms.len();
    dm.sync_to(300.0, 1.0).unwrap();
    pump(&mut dm, &mut recorder, "zoom back to 1", |recorder| {
        recorder.zooms.len() > reported
    });
    approx(*recorder.zooms.last().unwrap(), 1.0, 1e-3);
}

#[test]
fn unknown_pointers_cannot_be_contacts_and_stop_is_harmless() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let dm = rig(&window, DmConfig::default(), 1.0);
    let error = dm.set_contact(0x7777).unwrap_err();
    match error {
        WinError::Os { source, .. } => {
            assert_eq!(source.code(), HRESULT::from_win32(ERROR_NOT_FOUND.0));
        }
        other => panic!("unexpected error {other:?}"),
    }
    dm.stop().unwrap();
    assert!(!dm.status().unwrap().is_moving());
}

#[test]
fn activation_can_be_toggled() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let dm = rig(&window, DmConfig::default(), 1.0);
    dm.deactivate().unwrap();
    dm.activate().unwrap();
    assert!(!dm.status().unwrap().is_moving());
}

#[test]
fn a_dropped_viewport_leaves_the_window_free_for_the_next_one() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let first = rig(&window, DmConfig::default(), 1.0);
    first.sync_to(50.0, 1.0).unwrap();
    drop(first);
    let mut second = rig(&window, DmConfig::default(), 1.0);
    let mut recorder = Recorder::default();
    second.sync_to(70.0, 1.0).unwrap();
    pump(&mut second, &mut recorder, "scroll to 70", |recorder| {
        scrolled_to(recorder, 70.0)
    });
}

#[test]
fn wheel_input_animates_the_scroller_and_leaves_the_transform_stale() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    assert_eq!(driver.offset(), 0.0);
    assert!(!driver.has_contact());
    assert!(!driver.is_stale());
    assert_eq!(driver.scroller().extent(), Extent::new(300.0, 3000.0));
    assert_eq!(driver.scroller().animations(), client_area_animation());

    assert!(driver.on_wheel(&wheel(-1.0, false)).unwrap());
    assert!(driver.is_stale());
    let frames = run_frames(&mut driver, "wheel animation", |frame| !frame.more);
    assert!(is_non_decreasing(&frames));
    assert_eq!(driver.offset(), notch());
    assert!(frames.iter().all(|frame| frame.zoom.is_none()));
    if client_area_animation() {
        assert!(frames.len() > 3, "{} frames", frames.len());
        assert!(frames.iter().any(|frame| frame.changed));
    }

    // Pixel-precise input moves at once.
    assert!(driver.on_wheel(&wheel(-5.0, true)).unwrap());
    assert_eq!(driver.offset(), notch() + 5.0);
    // Programmatic scrolling.
    assert!(driver.jump_to(1000.0).unwrap());
    assert_eq!(driver.offset(), 1000.0);
    assert!(driver.scroll_to(1500.0).unwrap());
    run_frames(&mut driver, "scroll-to animation", |frame| !frame.more);
    assert_eq!(driver.offset(), 1500.0);
    // Nothing to do at the target.
    assert!(!driver.scroll_to(1500.0).unwrap());
}

#[test]
fn a_stale_transform_is_re_seated_before_a_contact_is_handed_over() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    driver.jump_to(700.0).unwrap();
    assert!(driver.is_stale());
    assert_eq!(
        driver.manipulation().content_transform().unwrap().offset_y,
        0.0
    );

    // The system does not know this pointer, so the contact is refused: but the content was
    // re-seated first, so the viewport and the scroller agree again.
    assert!(driver.start_contact(0x7777).is_err());
    assert!(!driver.is_stale());
    assert!(!driver.has_contact());
    approx(
        driver.manipulation().content_transform().unwrap().offset_y,
        700.0,
        0.01,
    );
    assert_eq!(driver.offset(), 700.0);
    // Not stale any more: the second attempt only fails at the contact.
    assert!(driver.start_contact(0x7777).is_err());
    assert_eq!(driver.offset(), 700.0);

    // A spring that is running when the contact arrives stops where it is.
    driver.scroll_to(1500.0).unwrap();
    if client_area_animation() {
        driver.frame(Duration::ZERO).unwrap();
        assert!(driver.scroller().is_animating());
        let frozen = driver.offset();
        assert!(driver.start_contact(0x7777).is_err());
        assert!(!driver.scroller().is_animating());
        assert_eq!(driver.offset(), frozen);
    }
}

#[test]
fn pointers_the_system_does_not_know_or_that_cannot_manipulate_are_ignored() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    assert!(
        !driver
            .on_pointer_message(WM_POINTERDOWN, WPARAM(0x7777))
            .unwrap()
    );
    assert!(
        !driver
            .on_pointer_message(DM_POINTERHITTEST, WPARAM(0x7777))
            .unwrap()
    );
    // A mouse pointer, or a message that does not start a contact, is left alone.
    assert!(
        !driver
            .on_pointer(WM_POINTERDOWN, 1, Some(PT_MOUSE))
            .unwrap()
    );
    assert!(!driver.on_pointer(WM_POINTERUP, 1, Some(PT_TOUCH)).unwrap());
    assert!(!driver.on_pointer(WM_POINTERDOWN, 1, None).unwrap());
    // A touch pointer is handed to DirectManipulation, which refuses the unknown id.
    assert!(
        driver
            .on_pointer(WM_POINTERDOWN, 0x7777, Some(PT_TOUCH))
            .is_err()
    );
    assert!(
        driver
            .on_pointer(DM_POINTERHITTEST, 0x7777, Some(PT_TOUCHPAD))
            .is_err()
    );
    assert!(!driver.has_contact());
}

#[test]
fn the_position_of_a_pointer_the_system_does_not_know_is_unavailable() {
    let window = HiddenWindow::new();
    assert_eq!(pointer_client_position(window.0, WPARAM(0x7777)), None);
    // Nor is it a pen: the window leaves unknown pointers to the normal input path.
    assert!(!pointer_is_pen(WPARAM(0x7777)));
}

#[test]
fn the_driver_adopts_what_directmanipulation_animates() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    driver.animate_to(800.0, 1.0).unwrap();
    let frames = run_frames(&mut driver, "DirectManipulation animation", |frame| {
        (frame.offset - 800.0).abs() < ANIMATION_SLACK && !frame.more
    });
    assert!(is_non_decreasing(&frames));
    assert!(frames.iter().filter(|frame| frame.changed).count() > 5);
    assert!(!driver.is_stale());
    assert_eq!(driver.scroller().excess(), 0.0);
    assert!(!driver.manipulation().status().unwrap().is_moving());
}

#[test]
fn a_wheel_notch_stops_a_running_manipulation_and_continues_from_where_it_was() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    driver.animate_to(2500.0, 1.0).unwrap();
    let frames = run_frames(&mut driver, "movement", |frame| frame.offset > 300.0);
    let before = frames.last().unwrap().offset;
    assert!(driver.manipulation().status().unwrap().is_moving());

    assert!(driver.on_wheel(&wheel(-1.0, false)).unwrap());
    assert!(!driver.manipulation().status().unwrap().is_moving());
    assert!(driver.is_stale());
    // The scroller continues from where DirectManipulation stopped.
    let stopped = driver.manipulation().content_transform().unwrap().offset_y;
    assert!(stopped >= before, "{stopped} < {before}");
    approx(driver.scroller().target(), stopped + notch(), 1e-3);
    run_frames(&mut driver, "wheel animation", |frame| !frame.more);
    approx(driver.offset(), stopped + notch(), 1e-3);
}

#[test]
fn zoom_from_directmanipulation_reaches_the_frame() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let config = DmConfig {
        zoom: Some(ZoomRange { min: 0.5, max: 4.0 }),
    };
    let mut driver = ScrollDriver::new(window.0, config, &layout(3000.0)).unwrap();
    driver.animate_to(300.0, 2.0).unwrap();
    let frames = run_frames(&mut driver, "zoomed animation", |frame| {
        (frame.offset - 300.0).abs() < ANIMATION_SLACK && !frame.more
    });
    let zooms: Vec<f64> = frames.iter().filter_map(|frame| frame.zoom).collect();
    assert!(!zooms.is_empty());
    approx(*zooms.last().unwrap(), 2.0, 1e-3);
}

#[test]
fn layout_changes_resize_the_viewport_and_clamp_the_offset() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    driver.jump_to(2000.0).unwrap();

    // The document shrinks to 1000 DIPs: the offset is pulled back to the new maximum.
    assert!(driver.set_layout(&layout(1000.0)).unwrap());
    assert_eq!(driver.offset(), 700.0);
    assert!(driver.is_stale());
    let dm = driver.manipulation();
    assert_eq!(dm.viewport_rect().unwrap().bottom, 300);
    assert_eq!(dm.content_rect().unwrap().bottom, 1000);

    // A DPI change: pixels double, DIP offsets stay.
    let hidpi = ScrollLayout {
        scale: 2.0,
        line_height: 25.0,
        ..layout(1000.0)
    };
    assert!(!driver.set_layout(&hidpi).unwrap());
    assert_eq!(driver.offset(), 700.0);
    let dm = driver.manipulation();
    assert_eq!(dm.viewport_rect().unwrap().right, 800);
    assert_eq!(dm.content_rect().unwrap().bottom, 2000);
    // The line height feeds the next wheel notch.
    driver.jump_to(0.0).unwrap();
    assert!(driver.on_wheel(&wheel(-2.0, true)).unwrap());
    assert_eq!(driver.offset(), 2.0);
    if let WheelLines::Lines(lines) = wheel_scroll_lines() {
        driver.jump_to(0.0).unwrap();
        driver.on_wheel(&wheel(-1.0, false)).unwrap();
        assert_eq!(driver.scroller().target(), f64::from(lines) * 25.0);
    }
}

#[test]
fn settings_and_activation_pass_through() {
    let _com = ComGuard::apartment_threaded().expect("STA");
    let window = HiddenWindow::new();
    let mut driver = ScrollDriver::new(window.0, DmConfig::default(), &layout(3000.0)).unwrap();
    // The system settings did not change since the driver read them.
    assert!(!driver.refresh_system_settings());
    assert_eq!(driver.scroller().animations(), client_area_animation());
    driver.deactivate().unwrap();
    assert!(!driver.has_contact());
    driver.activate().unwrap();
}
