//! The window procedure side of a shell window ([`WindowGlue`]): shortcuts, menu accelerators,
//! hit testing, painting, activation and what happens while the view is borrowed. The handler is
//! driven through the public `WindowHandler` trait with a hand-made `WindowInfo`; no native
//! window exists, what it asks of the loop owner is read back with `Controller::take_request`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, Request, WindowCell};
use ssw_app::ui::platform::Platform;
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::ui::window::{WindowEnv, WindowGlue, WindowLink, hit_region};
use ssw_core::command::CommandId;
use ssw_io::net::WinHttp;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::{Point, Size};
use ssw_ui::input::{Event, KeyEvent, Modifiers, PointerEvent, PointerEventKind, vk};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::layout::{CaptionButton, ChromeHit, ResizeEdge};
use ssw_ui::shell::titlebar::{MenuKind, TitleEvent};
use ssw_ui::shortcuts::CharProbe;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::Driver;
use ssw_win::taskbar::SystemTaskbars;
use ssw_win::window::current_os_version;
use ssw_win::window::{HitRegion, ResizeEdge as WinEdge, WindowHandler, WindowInfo};
use windows::Win32::Foundation::HWND;

/// A keyboard layout that answers every "does Ctrl+Alt+key type a character" the same way.
struct Layout(bool);

impl CharProbe for Layout {
    fn produces_char(&self, _vk: u16, _scan: u16, _shift: bool, _caps: bool) -> bool {
        self.0
    }
}

struct Host<B> {
    body: Option<B>,
}

impl<B: FnOnce(&Rc<Controller>)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = ScriptedServices::new();
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        (self.body.take().expect("the body runs once"))(&controller);
        0
    }
}

/// Runs `body` over the controller of a real startup in a temporary profile (no native window).
fn with_controller(body: impl FnOnce(&Rc<Controller>)) {
    let tmp = tempfile::tempdir().unwrap();
    let mut args = Args::default();
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let process = ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    };
    let mut dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process,
        &mut *dialogs,
        None,
        &mut Host { body: Some(body) },
    );
    assert_eq!(code, 0);
}

struct Fixture {
    controller: Rc<Controller>,
    cell: Rc<WindowCell>,
    env: Rc<WindowEnv>,
    link: Rc<WindowLink>,
    glue: WindowGlue,
}

/// The handler of a new window whose native window is never made.
fn fixture(controller: &Rc<Controller>, probe: Layout) -> Fixture {
    let cell = controller.create_window(None, Size::new(1280.0, 800.0), 1.0);
    while controller.take_request().is_some() {}
    let env = Rc::new(WindowEnv {
        controller: Rc::clone(controller),
        text: DWriteEngine::new().unwrap(),
        probe: Box::new(probe),
        mica: Cell::new(true),
        http: Arc::new(WinHttp),
        taskbar: Rc::new(SystemTaskbars),
    });
    let link = Rc::new(WindowLink::default());
    let platform = Platform::new(Rc::clone(&cell), Rc::clone(&env));
    let glue = WindowGlue::new(
        Rc::clone(&env),
        Rc::clone(&cell),
        Rc::clone(&link),
        platform,
    );
    // The first frame lays the title row out (menu buttons, caption buttons).
    cell.view.borrow_mut().paint(
        &mut RecordingCanvas::new(),
        &MonoTextEngine::default(),
        false,
    );
    Fixture {
        controller: Rc::clone(controller),
        cell,
        env,
        link,
        glue,
    }
}

fn info() -> WindowInfo {
    WindowInfo {
        hwnd: HWND::default(),
        dpi: 96,
        size_px: (1280, 800),
        maximized: false,
        active: true,
    }
}

fn key(fx: &mut Fixture, vk: u16, mods: Modifiers) {
    fx.glue
        .on_event(&info(), Event::Key(KeyEvent::press(vk, mods)));
}

/// The request that opens `menu` of the window of `fx` below its button.
fn open_menu(fx: &Fixture, menu: MenuKind) -> Request {
    let rect = fx.cell.view.borrow().titlebar().menu_rect(menu);
    Request::Action(
        fx.cell.id,
        ShellAction::Title(TitleEvent::MenuRequested {
            menu,
            anchor: Point::new(rect.x, rect.bottom()),
        }),
    )
}

#[test]
fn the_handler_and_its_environment_describe_themselves() {
    with_controller(|controller| {
        let fx = fixture(controller, Layout(false));
        assert_eq!(
            format!("{:?}", fx.env),
            format!("WindowEnv {{ mica: {}, .. }}", fx.env.mica.get())
        );
        assert_eq!(
            format!("{:?}", fx.glue),
            format!("WindowGlue {{ window: {:?}, .. }}", fx.cell.id)
        );
    });
}

#[test]
fn find_next_is_queued_as_a_command_of_the_window() {
    with_controller(|controller| {
        let mut fx = fixture(controller, Layout(false));
        key(&mut fx, vk::F3, Modifiers::NONE);
        assert_eq!(
            fx.controller.take_request(),
            Some(Request::Command(fx.cell.id, CommandId::SearchFindNext))
        );
        assert_eq!(fx.controller.take_request(), None);
    });
}

#[test]
fn f10_and_the_menu_letters_open_their_menus_below_their_buttons() {
    with_controller(|controller| {
        let mut fx = fixture(controller, Layout(false));
        key(&mut fx, vk::F10, Modifiers::NONE);
        assert_eq!(
            fx.controller.take_request(),
            Some(open_menu(&fx, MenuKind::File))
        );

        let mut anchors = Vec::new();
        for (letter, menu) in [
            (b'F', MenuKind::File),
            (b'E', MenuKind::Edit),
            (b'O', MenuKind::Format),
            (b'V', MenuKind::View),
            (b'W', MenuKind::Window),
            (b'H', MenuKind::Help),
        ] {
            key(&mut fx, u16::from(letter), Modifiers::ALT);
            let request = fx.controller.take_request();
            assert_eq!(
                request,
                Some(open_menu(&fx, menu)),
                "Alt+{}",
                letter as char
            );
            anchors.push(request);
        }
        for (i, first) in anchors.iter().enumerate() {
            for second in &anchors[i + 1..] {
                assert_ne!(first, second, "every menu opens at its own button");
            }
        }

        // A letter that is no menu's accelerator opens nothing.
        key(&mut fx, u16::from(b'Z'), Modifiers::ALT);
        assert_eq!(fx.controller.take_request(), None);
    });
}

#[test]
fn ctrl_alt_keys_that_type_a_character_are_text_and_queue_nothing() {
    with_controller(|controller| {
        let mut fx = fixture(controller, Layout(true));
        key(&mut fx, u16::from(b'Q'), Modifiers::CTRL | Modifiers::ALT);
        assert_eq!(fx.controller.take_request(), None);
    });
}

#[test]
fn a_view_that_is_borrowed_drops_input_answers_client_and_skips_the_paint() {
    with_controller(|controller| {
        let mut fx = fixture(controller, Layout(false));
        let view = fx.cell.view.borrow_mut();
        let point = Point::new(1250.0, 12.0);
        assert_eq!(fx.glue.hit_test(point, &info()), HitRegion::Client);

        // No anchor can be computed: the menu keys fall through to the (borrowed) view.
        fx.glue.on_event(
            &info(),
            Event::Key(KeyEvent::press(vk::F10, Modifiers::NONE)),
        );
        fx.glue.on_event(
            &info(),
            Event::Key(KeyEvent::press(u16::from(b'F'), Modifiers::ALT)),
        );
        fx.glue.on_event(
            &info(),
            Event::Pointer(PointerEvent::mouse(PointerEventKind::Move, None, point)),
        );
        assert_eq!(fx.controller.take_request(), None);

        let mut canvas = RecordingCanvas::new();
        fx.glue.on_paint(&info(), &mut canvas);
        assert!(canvas.recorded().is_empty());

        let mut small = info();
        small.size_px = (800, 600);
        fx.glue.on_resize(&small);
        assert!(fx.link.placement_dirty.get(), "the move is still noted");
        drop(view);
        assert_eq!(
            fx.cell.view.borrow().layout().size(),
            Size::new(1280.0, 800.0),
            "the borrowed view was not resized"
        );

        // Free again: the same calls work.
        assert_eq!(fx.glue.hit_test(point, &info()), HitRegion::Close);
        fx.glue.on_paint(&info(), &mut canvas);
        assert!(!canvas.recorded().is_empty());
        fx.glue.on_resize(&small);
        assert_eq!(
            fx.cell.view.borrow().layout().size(),
            Size::new(800.0, 600.0)
        );
    });
}

#[test]
fn hit_tests_follow_the_chrome_of_the_view() {
    with_controller(|controller| {
        let fx = fixture(controller, Layout(false));
        assert_eq!(
            fx.glue.hit_test(Point::new(1250.0, 12.0), &info()),
            HitRegion::Close
        );
        assert_eq!(
            fx.glue.hit_test(Point::new(640.0, 400.0), &info()),
            HitRegion::Client
        );
    });
}

#[test]
fn every_layout_hit_has_its_native_region() {
    assert_eq!(hit_region(ChromeHit::Client), HitRegion::Client);
    assert_eq!(hit_region(ChromeHit::Caption), HitRegion::Caption);
    assert_eq!(
        hit_region(ChromeHit::CaptionButton(CaptionButton::Minimize)),
        HitRegion::Minimize
    );
    assert_eq!(
        hit_region(ChromeHit::CaptionButton(CaptionButton::Maximize)),
        HitRegion::Maximize
    );
    assert_eq!(
        hit_region(ChromeHit::CaptionButton(CaptionButton::Close)),
        HitRegion::Close
    );
    for (edge, native) in [
        (ResizeEdge::Left, WinEdge::Left),
        (ResizeEdge::Right, WinEdge::Right),
        (ResizeEdge::Top, WinEdge::Top),
        (ResizeEdge::Bottom, WinEdge::Bottom),
        (ResizeEdge::TopLeft, WinEdge::TopLeft),
        (ResizeEdge::TopRight, WinEdge::TopRight),
        (ResizeEdge::BottomLeft, WinEdge::BottomLeft),
        (ResizeEdge::BottomRight, WinEdge::BottomRight),
    ] {
        assert_eq!(
            hit_region(ChromeHit::Resize(edge)),
            HitRegion::Resize(native)
        );
    }
}

#[test]
fn activating_a_window_makes_it_the_active_one_and_asks_for_a_file_check() {
    with_controller(|controller| {
        let first = fixture(controller, Layout(false));
        let mut second = fixture(controller, Layout(false));
        assert_eq!(controller.active_window().unwrap().id, first.cell.id);

        second.glue.on_activate(&info(), false);
        assert_eq!(controller.active_window().unwrap().id, first.cell.id);
        assert_eq!(controller.take_request(), None);

        second.glue.on_activate(&info(), true);
        assert_eq!(controller.active_window().unwrap().id, second.cell.id);
        assert_eq!(
            controller.take_request(),
            Some(Request::Activated(second.cell.id))
        );
    });
}
