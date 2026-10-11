//! The window content (`ShellView`) over a started application in a temporary profile: pane
//! cycling, the splitter drag, the hidden chrome, tab reordering, zoom and the pill. Nothing is
//! shown: events are delivered to the view and what it reports or paints is read back.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::shell_view::{ShellAction, ShellView, ViewConfig, sidebar_options};
use ssw_core::command::CommandId;
use ssw_core::settings::{Settings, SortBy};
use ssw_ui::access::AccessAction;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::EditorEvent;
use ssw_ui::geometry::{Point, Size};
use ssw_ui::input::{Event, FocusEvent, PointerButton, PointerEvent, PointerEventKind, TextInput};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::layout::{ChromeHit, Pane, SIDEBAR_MAX_WIDTH, ShellMode};
use ssw_ui::shell::sidebar::SortKey;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::{Control, SharedClipboard};
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

struct Rig {
    controller: Rc<Controller>,
    engine: MonoTextEngine,
    work: std::path::PathBuf,
}

struct Host<B> {
    body: Option<B>,
    work: std::path::PathBuf,
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
            work: self.work.clone(),
        };
        (self.body.take().expect("the body runs once"))(&rig);
        0
    }
}

fn with_rig(files: &[(&str, &str)], body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    for (name, text) in files {
        let path = work.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), NO_AUTOSAVE).unwrap();
    let mut args = Args {
        paths: vec![work.join(files[0].0)],
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
        &mut Host {
            body: Some(body),
            work,
        },
    );
    assert_eq!(code, 0);
}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn paint(&self) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        self.window()
            .view
            .borrow_mut()
            .paint(&mut canvas, &self.engine, false);
        canvas
    }

    /// Paints a view the caller already holds borrowed.
    fn paint_view(&self, view: &mut ShellView) {
        view.paint(&mut RecordingCanvas::new(), &self.engine, false);
    }

    fn pointer(&self, kind: PointerEventKind, at: Point) {
        let event = PointerEvent {
            click_count: 1,
            ..PointerEvent::mouse(kind, Some(PointerButton::Left), at)
        };
        let window = self.window();
        let actions = window
            .view
            .borrow_mut()
            .event(&Event::Pointer(event), &self.engine);
        for action in actions {
            self.controller.handle_action(window.id, action);
        }
    }
}

#[test]
fn the_sidebar_options_follow_every_sort_order_of_the_settings() {
    for (by, key) in [
        (SortBy::DateModified, SortKey::Modified),
        (SortBy::DateCreated, SortKey::Created),
        (SortBy::Kind, SortKey::Kind),
        (SortBy::Name, SortKey::Name),
    ] {
        let mut settings = Settings::default();
        settings.sidebar.sort_by = by;
        assert_eq!(sidebar_options(&settings).sort, key);
    }
}

#[test]
fn views_and_their_configuration_describe_themselves() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let config = ViewConfig {
            localizer: Rc::clone(rig.controller.i18n()),
            theme: rig.controller.theme(),
            size: Size::new(640.0, 480.0),
            scale: 1.5,
            clock: Rc::new(FakeClock::default()),
            clipboard: Rc::new(RefCell::new(MemoryClipboard::default())),
            settings: rig.controller.settings().clone(),
        };
        let text = format!("{config:?}");
        assert!(text.starts_with("ViewConfig { size:"), "{text}");
        assert!(text.contains("scale: 1.5"), "{text}");
        // A view without tabs has no plain-text file to wrap or render.
        let mut empty = ShellView::new(config);
        assert!(!empty.toggle_word_wrap());
        assert!(!empty.toggle_render_as_markdown());
        // An overlay closing while nothing has the focus hands the keyboard to the editor pane.
        empty.overlays_mut().close_folder_search();
        rig.paint_view(&mut empty);
        assert_eq!(empty.focused_pane(), Pane::Editor);
        let window = rig.window();
        let text = format!("{:?}", window.view.borrow());
        assert!(text.starts_with("ShellView { layout:"), "{text}");
        assert!(text.contains("tabs: 1"), "{text}");
        assert!(text.contains("active: Some(0)"), "{text}");
    });
}

#[test]
fn tabs_can_be_reordered_the_active_one_stays_and_unknown_tabs_are_refused() {
    with_rig(&[("a.md", "A\n"), ("b.md", "B\n")], |rig| {
        let window = rig.window();
        rig.controller
            .open_file(&window, &rig.work.join("b.md"))
            .unwrap();
        let mut view = window.view.borrow_mut();
        let keys: Vec<u64> = view.tabs().iter().map(|tab| tab.key).collect();
        assert_eq!(keys.len(), 2);
        let active = view.active_tab().unwrap().key;
        assert_eq!(active, keys[1]);
        view.reorder_tab(1, 0);
        let moved: Vec<u64> = view.tabs().iter().map(|tab| tab.key).collect();
        assert_eq!(moved, [keys[1], keys[0]]);
        assert_eq!(
            view.active_tab().unwrap().key,
            active,
            "still the active tab"
        );
        view.reorder_tab(0, 5);
        view.reorder_tab(7, 0);
        let same: Vec<u64> = view.tabs().iter().map(|tab| tab.key).collect();
        assert_eq!(same, moved, "out of range moves nothing");
        assert!(!view.activate_tab(9_999));
        assert!(view.close_tab(9_999).is_none());
        assert_eq!(view.tabs().len(), 2);
    });
}

#[test]
fn the_zoom_and_the_pill_reach_every_tab_and_the_title_bar() {
    with_rig(&[("a.md", "A\n"), ("b.md", "B\n")], |rig| {
        let window = rig.window();
        rig.controller
            .open_file(&window, &rig.work.join("b.md"))
            .unwrap();
        let mut view = window.view.borrow_mut();
        view.set_zoom_percent(150);
        assert!(
            view.tabs()
                .iter()
                .all(|tab| tab.editor.zoom_percent() == 150)
        );
        let shown = view.titlebar().pill_visible();
        view.toggle_format_pill();
        assert_eq!(view.titlebar().pill_visible(), !shown);
        view.toggle_format_pill();
        assert_eq!(view.titlebar().pill_visible(), shown);
        let options = view.sidebar_options();
        view.set_sidebar_options(options);
        assert_eq!(view.sidebar_options(), options);
    });
}

#[test]
fn hidden_chrome_is_neither_laid_out_nor_painted_nor_hit() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let window = rig.window();
        let shown = rig.paint().texts().len();
        let at_caption = Point::new(640.0, 10.0);
        assert_eq!(
            window.view.borrow().chrome_hit(at_caption),
            ChromeHit::Caption
        );

        window
            .view
            .borrow_mut()
            .layout_mut()
            .set_status_bar_visible(false);
        let without_status = rig.paint().texts().len();
        assert!(without_status < shown, "{without_status} < {shown}");

        window.view.borrow_mut().set_mode(ShellMode::Zen);
        let zen = rig.paint().texts().len();
        assert!(zen < without_status, "{zen} < {without_status}");
        assert_ne!(
            window.view.borrow().chrome_hit(at_caption),
            ChromeHit::Caption
        );
    });
}

#[test]
fn f6_moves_the_focus_through_the_visible_panes_in_order() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let window = rig.window();
        rig.controller.show_workspace(&window, &rig.work);
        {
            let mut view = window.view.borrow_mut();
            if !view.layout().inspector_visible() {
                view.toggle_inspector();
            }
        }
        rig.paint();
        let mut view = window.view.borrow_mut();
        assert_eq!(view.focused_pane(), Pane::Editor);
        assert!(view.layout().panes().contains(&Pane::CommandRow));
        let mut step = |forward: bool| {
            let pane = view.next_pane(forward);
            assert_eq!(view.focused_pane(), pane, "the focus is where F6 said");
            pane
        };
        // The title row and the command row are one stop of the cycle: the title bar owns the
        // focus of both.
        let forward: Vec<Pane> = (0..5).map(|_| step(true)).collect();
        assert_eq!(
            forward,
            [
                Pane::Inspector,
                Pane::StatusBar,
                Pane::TitleRow,
                Pane::Sidebar,
                Pane::Editor
            ],
            "the cycle ends where it began"
        );
        let backward: Vec<Pane> = (0..5).map(|_| step(false)).collect();
        assert_eq!(
            backward,
            [
                Pane::Sidebar,
                Pane::TitleRow,
                Pane::StatusBar,
                Pane::Inspector,
                Pane::Editor
            ]
        );
    });
}

#[test]
fn f6_still_visits_the_sidebar_and_the_editor_when_there_is_no_workspace_and_no_document() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let fresh = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        let mut view = fresh.view.borrow_mut();
        if !view.layout().sidebar_visible() {
            view.toggle_sidebar();
        }
        rig.paint_view(&mut view);
        assert!(view.sidebar().is_none() && view.active_tab().is_none());
        assert!(view.layout().panes().contains(&Pane::Sidebar));
        // The panes stay stops of the cycle although neither has a widget to take the focus.
        let visited: Vec<Pane> = (0..4)
            .map(|_| {
                let pane = view.next_pane(true);
                assert_eq!(view.focused_pane(), pane, "the focus is where F6 said");
                pane
            })
            .collect();
        assert_eq!(
            visited,
            [Pane::StatusBar, Pane::TitleRow, Pane::Sidebar, Pane::Editor]
        );
    });
}

#[test]
fn a_rename_of_an_entry_the_sidebar_does_not_show_opens_no_rename_box_and_keeps_the_focus() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let window = rig.window();
        rig.controller.show_workspace(&window, &rig.work);
        rig.paint();
        let mut view = window.view.borrow_mut();
        assert_eq!(view.focused_pane(), Pane::Editor);

        view.request_rename(u64::MAX);
        rig.paint_view(&mut view);
        assert_eq!(view.sidebar().expect("a sidebar").renaming(), None);
        assert_eq!(view.focused_pane(), Pane::Editor);

        // An entry that is shown gets its rename box, and the sidebar the focus.
        let key = view
            .sidebar()
            .and_then(|sidebar| sidebar.row_key(0))
            .expect("the sidebar shows a row");
        view.request_rename(key);
        rig.paint_view(&mut view);
        assert_eq!(view.sidebar().expect("a sidebar").renaming(), Some(key));
        assert_eq!(view.focused_pane(), Pane::Sidebar);
    });
}

#[test]
fn the_splitter_follows_the_pointer_while_it_is_dragged() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let window = rig.window();
        rig.controller.show_workspace(&window, &rig.work);
        rig.paint();
        let sidebar = window
            .view
            .borrow()
            .layout()
            .rects()
            .sidebar
            .expect("a sidebar");
        let edge = Point::new(sidebar.x + sidebar.width, 300.0);
        // Hovering the edge (the resize cursor) does not start a drag; pressing it does.
        rig.pointer(PointerEventKind::Move, edge);
        assert!(window.view.borrow().layout().dragging().is_none());
        rig.pointer(PointerEventKind::Down, edge);
        assert!(window.view.borrow().layout().dragging().is_some());
        rig.pointer(PointerEventKind::Move, Point::new(edge.x + 60.0, 300.0));
        let wider = window
            .view
            .borrow()
            .layout()
            .rects()
            .sidebar
            .expect("a sidebar");
        assert!(
            wider.width > sidebar.width,
            "{} > {}",
            wider.width,
            sidebar.width
        );
        rig.pointer(PointerEventKind::Leave, Point::new(edge.x + 60.0, 300.0));
        assert!(window.view.borrow().layout().dragging().is_some());
        rig.pointer(PointerEventKind::Up, Point::new(edge.x + 60.0, 300.0));
        assert!(window.view.borrow().layout().dragging().is_none());
    });
}

#[test]
fn a_splitter_dragged_past_its_limit_stays_at_the_limit() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let window = rig.window();
        rig.controller.show_workspace(&window, &rig.work);
        rig.paint();
        let sidebar = window
            .view
            .borrow()
            .layout()
            .rects()
            .sidebar
            .expect("a sidebar");
        let width = || window.view.borrow().layout().sidebar_width();
        rig.pointer(
            PointerEventKind::Down,
            Point::new(sidebar.x + sidebar.width, 300.0),
        );
        rig.pointer(PointerEventKind::Move, Point::new(900.0, 300.0));
        assert_eq!(width(), SIDEBAR_MAX_WIDTH);
        // Dragging on past the limit changes nothing: the pane stays as wide as it can be.
        rig.pointer(PointerEventKind::Move, Point::new(1000.0, 300.0));
        assert_eq!(width(), SIDEBAR_MAX_WIDTH);
        rig.pointer(PointerEventKind::Up, Point::new(1000.0, 300.0));
        assert!(window.view.borrow().layout().dragging().is_none());
    });
}

#[test]
fn pointers_crossing_panes_leave_the_old_one_and_outside_the_window_hits_nothing() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        rig.paint();
        let window = rig.window();
        rig.pointer(PointerEventKind::Move, Point::new(640.0, 400.0));
        rig.pointer(PointerEventKind::Move, Point::new(640.0, 10.0));
        rig.pointer(PointerEventKind::Move, Point::new(-50.0, -50.0));
        assert!(window.view.borrow().layout().dragging().is_none());
    });
}

#[test]
fn a_window_without_tabs_ignores_text_and_focus_events() {
    with_rig(&[("a.md", "# A\n")], |rig| {
        let fresh = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        let mut view = fresh.view.borrow_mut();
        let text = Event::Text(TextInput("x".to_owned()));
        assert!(view.event(&text, &rig.engine).is_empty());
        assert!(
            view.event(&Event::Focus(FocusEvent::Gained), &rig.engine)
                .is_empty()
        );
        assert!(!view.run_table_command(CommandId::FileNew));
    });
}

#[test]
fn the_link_command_opens_the_link_popup_over_the_active_editor() {
    with_rig(&[("a.md", "[text](target.md)\n")], |rig| {
        let window = rig.window();
        assert!(!window.view.borrow().overlays().link_visible());
        let _ = rig.controller.run_command(window.id, CommandId::FormatLink);
        assert!(window.view.borrow().overlays().link_visible());
    });
}

#[test]
fn the_link_command_has_nothing_to_edit_in_a_window_without_a_document() {
    with_rig(&[("a.md", "[text](target.md)\n")], |rig| {
        let fresh = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        let _ = rig.controller.run_command(fresh.id, CommandId::FormatLink);
        assert!(!fresh.view.borrow().overlays().link_visible());
    });
}

#[test]
fn what_the_editor_of_a_background_tab_reports_reaches_the_controller_under_its_own_key() {
    with_rig(&[("a.md", "A\n"), ("b.md", "B\n")], |rig| {
        let window = rig.window();
        rig.controller
            .open_file(&window, &rig.work.join("b.md"))
            .unwrap();
        let mut view = window.view.borrow_mut();
        let background = view.tabs()[0].key;
        assert_ne!(Some(background), view.active_tab().map(|tab| tab.key));

        // The text of a tab that is not shown changes (a drop from another window, say).
        view.tabs_mut()[0].editor.insert_date("1 January 2000");
        let actions = view.event(&Event::AnimationFrame(Duration::ZERO), &rig.engine);
        let reported: Vec<&ShellAction> = actions
            .iter()
            .filter(|action| matches!(action, ShellAction::Editor(..)))
            .collect();
        assert_eq!(
            reported,
            [&ShellAction::Editor(background, EditorEvent::Edited)]
        );
    });
}

#[test]
fn assistive_technology_acts_on_the_tabs_and_the_editor_and_sees_the_inspector() {
    let text: String = (0..300)
        .map(|n| format!("Paragraph {n} of the text.\n\n"))
        .collect();
    with_rig(&[("a.md", text.as_str()), ("b.md", "B\n")], |rig| {
        let window = rig.window();
        rig.controller
            .open_file(&window, &rig.work.join("b.md"))
            .unwrap();
        {
            let mut view = window.view.borrow_mut();
            if !view.layout().inspector_visible() {
                view.toggle_inspector();
            }
        }
        rig.paint();
        let mut view = window.view.borrow_mut();
        let tree = view.access_tree(&rig.engine, "Window");
        assert!(
            tree.node(view.inspector().id()).is_some(),
            "the inspector is part of the tree while it is shown"
        );

        // Selecting the tab item of the first tab (b.md is active) is the title bar's business.
        let tabs = tree
            .node(view.titlebar().tabs().id())
            .expect("the tab strip");
        let first_item = tabs.children[0];
        let actions = view.perform_access(first_item, &AccessAction::Select, &rig.engine);
        assert!(!actions.is_empty(), "the tab item reported nothing");
        assert!(
            actions
                .iter()
                .all(|action| matches!(action, ShellAction::Title(_))),
            "{actions:?}"
        );

        // Anything else that is not a title bar item goes to the editor of the active tab: the
        // long document of a.md.
        let first_key = view.tabs()[0].key;
        assert!(view.activate_tab(first_key));
        rig.paint_view(&mut view);
        let editor = view.active_tab().unwrap().editor.id();
        view.perform_access(
            editor,
            &AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(100.0),
            },
            &rig.engine,
        );
        assert!(view.active_tab().unwrap().editor.scroll_y() > 0.0);
        // A node that is neither is left alone.
        let before = view.active_tab().unwrap().editor.scroll_y();
        let stranger = view.root_id();
        assert!(
            view.perform_access(stranger, &AccessAction::Invoke, &rig.engine)
                .is_empty()
        );
        assert_eq!(view.active_tab().unwrap().editor.scroll_y(), before);
    });
}
