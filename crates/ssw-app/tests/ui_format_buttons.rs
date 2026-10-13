//! The B I U pill and the selection toolbar format the selection the way the shortcut does,
//! keep the selection and leave the keyboard with the editor (spec §2.2, §2.9, §3.2), and are
//! not offered where there is no Markdown to write (spec §4.1).

use std::cell::RefCell;
use std::rc::Rc;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind, TextInput};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::layout::Pane;
use ssw_ui::shell::titlebar::FormatToggle;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widget::TimerRequest;
use ssw_ui::widgets::SharedClipboard;
use ssw_ui::widgets::tooltip::TOOLTIP_DELAY;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

struct Rig {
    controller: Rc<Controller>,
    engine: MonoTextEngine,
}

struct Host<B> {
    body: Option<B>,
}

impl<B: FnOnce(&Rig)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = ScriptedServices::new();
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            engine: MonoTextEngine::default(),
        };
        (self.body.take().expect("the body runs once"))(&rig);
        0
    }
}

fn with_rig(file: &str, text: &str, body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join(file), text).unwrap();
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), NO_AUTOSAVE).unwrap();
    let mut args = Args {
        paths: vec![work.join(file)],
        ..Args::default()
    };
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

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn layout(&self) {
        self.window().view.borrow_mut().paint(
            &mut ssw_ui::canvas::RecordingCanvas::new(),
            &self.engine,
            false,
        );
    }

    fn deliver(&self, event: Event) {
        let window = self.window();
        let actions = window.view.borrow_mut().event(&event, &self.engine);
        for action in actions {
            self.controller.handle_action(window.id, action);
        }
    }

    fn pointer(&self, kind: PointerEventKind, at: Point) {
        let button = matches!(kind, PointerEventKind::Down | PointerEventKind::Up)
            .then_some(PointerButton::Left);
        self.deliver(Event::Pointer(PointerEvent {
            click_count: 1,
            ..PointerEvent::mouse(kind, button, at)
        }));
    }

    fn click(&self, at: Point) {
        self.layout();
        self.pointer(PointerEventKind::Move, at);
        self.pointer(PointerEventKind::Down, at);
        self.pointer(PointerEventKind::Up, at);
    }

    fn type_text(&self, text: &str) {
        self.deliver(Event::Text(TextInput(text.to_owned())));
    }

    fn select(&self, selection: Selection) {
        self.window()
            .view
            .borrow_mut()
            .active_editor_mut()
            .expect("an editor")
            .go_to(selection);
    }

    fn text(&self) -> String {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab().unwrap().entry.document().borrow().text()
    }

    fn selection(&self) -> Selection {
        let window = self.window();
        let view = window.view.borrow();
        view.active_tab()
            .unwrap()
            .entry
            .document()
            .borrow()
            .primary_selection()
    }

    fn pill_item(&self, toggle: FormatToggle) -> Point {
        self.layout();
        self.window()
            .view
            .borrow()
            .titlebar()
            .pill_item_rect(toggle)
            .center()
    }
}

impl Rig {
    /// Drags over the text of the first line with the mouse, so the toolbar shows.
    fn drag_select(&self) {
        self.layout();
        let window = self.window();
        let caret = {
            let mut view = window.view.borrow_mut();
            view.active_editor_mut().unwrap().go_to(Selection::caret(0));
            view.active_tab().unwrap().editor.caret_rect().unwrap()
        };
        self.layout();
        let y = caret.center().y;
        self.pointer(PointerEventKind::Move, Point::new(caret.x + 1.0, y));
        self.pointer(PointerEventKind::Down, Point::new(caret.x + 1.0, y));
        self.pointer(PointerEventKind::Move, Point::new(caret.x + 60.0, y));
        self.pointer(PointerEventKind::Move, Point::new(caret.x + 200.0, y));
        self.pointer(PointerEventKind::Up, Point::new(caret.x + 200.0, y));
    }

    fn toolbar_button(&self, name: &str) -> Point {
        self.layout();
        let window = self.window();
        let view = window.view.borrow();
        let focus = ssw_ui::widget::FocusManager::new();
        let theme = ssw_ui::theme::Theme::light(ssw_ui::theme::DEFAULT_ACCENT);
        let root = ssw_ui::widget::WidgetId::allocate();
        let cx = ssw_ui::widget::AccessCx::new(root, &theme, 1.0, &self.engine, &focus);
        let mut tree = ssw_ui::access::AccessTree::new(root);
        view.overlays().access(&cx, &mut tree);
        tree.nodes()
            .iter()
            .find(|node| node.role == ssw_ui::access::Role::Button && node.name == name)
            .unwrap_or_else(|| panic!("no button named {name:?}"))
            .bounds
            .center()
    }
}

/// What the document and the selection are after `FormatBold` ran as the shortcut would.
fn after_shortcut(file: &str, text: &str, selection: Selection) -> (String, Selection) {
    let mut result = None;
    with_rig(file, text, |rig| {
        rig.layout();
        rig.select(selection);
        let _ = rig
            .controller
            .run_command(rig.window().id, CommandId::FormatBold);
        result = Some((rig.text(), rig.selection()));
    });
    result.expect("the body ran")
}

#[test]
fn the_pill_formats_the_selection_like_the_shortcut_and_the_editor_keeps_the_keyboard() {
    let expected = after_shortcut("a.md", "hello world\n", Selection::new(6, 11));
    assert_eq!(expected.0, "hello **world**\n");
    with_rig("a.md", "hello world\n", |rig| {
        rig.layout();
        rig.select(Selection::new(6, 11));
        assert!(rig.window().view.borrow().titlebar().format_enabled());
        rig.click(rig.pill_item(FormatToggle::Bold));
        assert_eq!((rig.text(), rig.selection()), expected);
        assert!(!rig.selection().is_caret());
        assert_eq!(rig.window().view.borrow().focused_pane(), Pane::Editor);
        // Typing after the click replaces the selection: the editor still has the keyboard.
        rig.type_text("x");
        assert_eq!(rig.text(), "hello **x**\n");
    });
}

#[test]
fn the_toolbar_formats_the_selection_like_the_shortcut_and_the_editor_keeps_the_keyboard() {
    let expected = after_shortcut("a.md", "hello world\n", Selection::new(0, 11));
    assert_eq!(expected.0, "**hello world**\n");
    with_rig("a.md", "hello world\n", |rig| {
        rig.drag_select();
        assert_eq!(rig.selection(), Selection::new(0, 11));
        assert!(rig.window().view.borrow().overlays().toolbar_visible());
        let at = rig.toolbar_button("Bold");
        rig.click(at);
        assert_eq!((rig.text(), rig.selection()), expected);
        assert!(!rig.selection().is_caret());
        rig.type_text("x");
        assert_eq!(rig.text(), "**x**\n");
    });
}

#[test]
fn a_plain_text_document_has_no_formatting_buttons_to_press() {
    with_rig("a.txt", "hello world\n", |rig| {
        rig.layout();
        rig.select(Selection::new(6, 11));
        let id = rig.window().id;
        assert!(matches!(
            rig.controller.run_command(id, CommandId::FormatBold),
            ssw_app::commands::Dispatch::Unhandled
        ));
        // The pill is disabled, so the click does nothing and nothing is silently dead.
        rig.layout();
        assert!(!rig.window().view.borrow().titlebar().format_enabled());
        rig.click(rig.pill_item(FormatToggle::Bold));
        assert_eq!(rig.text(), "hello world\n");
        // A mouse selection shows no toolbar.
        rig.drag_select();
        assert_eq!(rig.selection(), Selection::new(0, 11));
        assert!(!rig.window().view.borrow().overlays().toolbar_visible());
    });
}

impl Rig {
    /// Rests the pointer at `at`, fires the tooltip timer the view asked for and returns what
    /// the tooltip shows.
    fn hover(&self, at: Point) -> Option<String> {
        self.layout();
        self.pointer(PointerEventKind::Move, at);
        let timer = {
            let window = self.window();
            let mut view = window.view.borrow_mut();
            view.requests_mut()
                .timers()
                .iter()
                .rev()
                .find_map(|request| match request {
                    TimerRequest::Set { id, interval, .. } if *interval == TOOLTIP_DELAY => {
                        Some(*id)
                    }
                    _ => None,
                })
        };
        if let Some(id) = timer {
            self.deliver(Event::Timer(id));
        }
        let window = self.window();
        let view = window.view.borrow();
        view.tooltip()
            .is_visible()
            .then(|| view.tooltip().text().to_owned())
    }

    fn status_button(&self, hint: &str) -> Point {
        self.layout();
        let window = self.window();
        let view = window.view.borrow();
        let tree = view.access_tree(&self.engine, "Window");
        tree.nodes()
            .iter()
            .find(|node| node.description == hint)
            .unwrap_or_else(|| panic!("no node described {hint:?}"))
            .bounds
            .center()
    }
}

#[test]
fn a_pointer_resting_on_an_icon_button_shows_its_name_and_leaving_hides_it() {
    with_rig("a.md", "hello world\n", |rig| {
        rig.layout();
        let nowhere = Point::new(400.0, 300.0);
        assert_eq!(rig.hover(nowhere), None);
        // The pill, the pane toggles and the status bar's clickable segments.
        let (sidebar, inspector) = {
            let window = rig.window();
            let view = window.view.borrow();
            (
                view.titlebar().sidebar_toggle_rect().center(),
                view.titlebar().inspector_toggle_rect().center(),
            )
        };
        for (at, name) in [
            (rig.pill_item(FormatToggle::Bold), "Bold"),
            (rig.pill_item(FormatToggle::Italic), "Italic"),
            (rig.pill_item(FormatToggle::Underline), "Underline"),
            (sidebar, "Sidebar"),
            (inspector, "Inspector"),
            (rig.status_button("Change encoding"), "Change encoding"),
            (
                rig.status_button("Change line endings"),
                "Change line endings",
            ),
        ] {
            assert_eq!(rig.hover(at).as_deref(), Some(name), "{at:?}");
            // Leaving the control hides the tooltip.
            rig.pointer(PointerEventKind::Move, nowhere);
            assert!(!rig.window().view.borrow().tooltip().is_visible(), "{name}");
        }
        // A press hides it too.
        assert_eq!(
            rig.hover(rig.pill_item(FormatToggle::Bold)).as_deref(),
            Some("Bold")
        );
        rig.pointer(PointerEventKind::Down, rig.pill_item(FormatToggle::Bold));
        assert!(!rig.window().view.borrow().tooltip().is_visible());
    });
}

#[test]
fn the_sidebars_buttons_show_their_names_until_the_pane_is_hidden() {
    with_rig("a.md", "hello world\n", |rig| {
        // Showing the sidebar loads the folder of the file.
        rig.controller
            .run_command(rig.window().id, CommandId::ViewToggleSidebar);
        rig.layout();
        let new_button = {
            let window = rig.window();
            let view = window.view.borrow();
            let pane = view.sidebar().expect("the folder pane").bounds();
            let tree = view.access_tree(&rig.engine, "Window");
            tree.nodes()
                .iter()
                .find(|node| {
                    node.role == ssw_ui::access::Role::Button
                        && node.name == "New"
                        && pane.contains(node.bounds.center())
                })
                .expect("the New button of the folder pane")
                .bounds
                .center()
        };
        assert_eq!(rig.hover(new_button).as_deref(), Some("New"));

        // Hiding it keeps the pane's widget, which is not asked without a rectangle.
        rig.controller
            .run_command(rig.window().id, CommandId::ViewToggleSidebar);
        rig.layout();
        assert!(!rig.window().view.borrow().layout().sidebar_visible());
        assert_eq!(rig.hover(new_button), None);
    });
}

#[test]
fn the_selection_toolbar_buttons_show_their_names() {
    with_rig("a.md", "hello world\n", |rig| {
        rig.drag_select();
        for name in ["Bold", "Italic", "Link", "Heading"] {
            let at = rig.toolbar_button(name);
            assert_eq!(rig.hover(at).as_deref(), Some(name));
        }
    });
}
