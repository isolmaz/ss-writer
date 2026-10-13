//! Search the way the user does it: the shortcut opens the find bar or the folder search, the
//! pattern is typed with key events into the window content (the same path the window procedure
//! takes: the shortcut router first, then the view), and what the user sees is read back (the
//! document's selection, the matches, the results list, what the canvas drew). Nothing calls a
//! command to type or a setter to fill a box.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_core::edit::Selection;
use ssw_ui::access::{AccessTree, Role};
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::geometry::Point;
use ssw_ui::input::{
    Event, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, TextInput, vk,
};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shortcuts::{CharProbe, ShortcutOutcome};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{AccessCx, FocusManager, WidgetId};
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

struct NoAltGr;

impl CharProbe for NoAltGr {
    fn produces_char(&self, _vk: u16, _scan: u16, _shift: bool, _caps: bool) -> bool {
        false
    }
}

struct Rig {
    controller: Rc<Controller>,
    engine: MonoTextEngine,
    work: PathBuf,
}

struct Host<B> {
    body: Option<B>,
    work: PathBuf,
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

/// Runs `body` over a started application whose first window shows `files[0]`.
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

    /// Lays the window out and paints it; returns what was drawn.
    fn paint(&self) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        self.window()
            .view
            .borrow_mut()
            .paint(&mut canvas, &self.engine, false);
        canvas
    }

    /// Hands `event` to the window content and carries out what the user asked for.
    fn deliver(&self, event: Event) {
        let window = self.window();
        let actions = window.view.borrow_mut().event(&event, &self.engine);
        for action in actions {
            self.controller.handle_action(window.id, action);
        }
    }

    /// A key press the way the window procedure handles it: the shortcut router first, the
    /// view when no shortcut took the key.
    fn press(&self, key: u16, mods: Modifiers) {
        let event = KeyEvent::press(key, mods);
        match self.controller.route_key(&event, &NoAltGr, false) {
            ShortcutOutcome::Command(command) => {
                self.controller.run_command(self.window().id, command);
            }
            ShortcutOutcome::Unhandled => self.deliver(Event::Key(event)),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// One text event per character, like typing.
    fn type_text(&self, text: &str) {
        for character in text.chars() {
            self.deliver(Event::Text(TextInput(character.to_string())));
        }
    }

    fn click(&self, at: Point) {
        self.paint();
        for kind in [
            PointerEventKind::Move,
            PointerEventKind::Down,
            PointerEventKind::Up,
        ] {
            let button = (kind != PointerEventKind::Move).then_some(PointerButton::Left);
            self.deliver(Event::Pointer(PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, button, at)
            }));
        }
    }

    /// The center of the overlay control `role` whose name contains `name`, after a layout.
    fn center_of(&self, role: Role, name: &str) -> Point {
        self.paint();
        let window = self.window();
        let view = window.view.borrow();
        let focus = FocusManager::new();
        let theme = Theme::light(DEFAULT_ACCENT);
        let root = WidgetId::allocate();
        let cx = AccessCx::new(root, &theme, 1.0, &self.engine, &focus);
        let mut tree = AccessTree::new(root);
        view.overlays().access(&cx, &mut tree);
        tree.nodes()
            .iter()
            .find(|node| node.role == role && node.name.contains(name))
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
            .bounds
            .center()
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

    fn matches(&self) -> usize {
        self.window()
            .view
            .borrow()
            .overlays()
            .find()
            .matches()
            .len()
    }

    fn found(&self) -> Vec<String> {
        self.window()
            .view
            .borrow()
            .overlays()
            .folder_search()
            .files()
            .iter()
            .map(|file| file.path.replace('\\', "/"))
            .collect()
    }

    /// Delivers worker results until nothing is pending.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.controller.core().borrow_mut().pump();
            self.controller.refresh_all();
            if self.controller.core().borrow().workers.pending() == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the workers did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[test]
fn typing_into_the_find_bar_finds_selects_and_steps_through_the_matches() {
    with_rig(&[("notes.md", "one two one\n")], |rig| {
        rig.press(u16::from(b'F'), Modifiers::CTRL);
        assert!(rig.window().view.borrow().overlays().find_visible());

        rig.type_text("one");
        assert_eq!(rig.matches(), 2);
        assert_eq!(rig.selection().range(), (0..3).into());
        let counter = rig.window().view.borrow().overlays().find().counter_text();
        assert_eq!(counter, "1/2");
        assert!(rig.paint().texts().contains(&"1/2"));

        // Enter steps to the next match, Shift+Enter back, F3 on again.
        rig.press(vk::RETURN, Modifiers::NONE);
        assert_eq!(rig.selection().range(), (8..11).into());
        rig.press(vk::RETURN, Modifiers::SHIFT);
        assert_eq!(rig.selection().range(), (0..3).into());
        rig.press(vk::F3, Modifiers::NONE);
        assert_eq!(rig.selection().range(), (8..11).into());

        // Escape closes the bar and the keyboard returns to the editor.
        rig.press(vk::ESCAPE, Modifiers::NONE);
        assert!(!rig.window().view.borrow().overlays().find_visible());
        rig.type_text("!");
        let window = rig.window();
        let view = window.view.borrow();
        let text = view.active_tab().unwrap().entry.document().borrow().text();
        assert_eq!(
            text, "one two !\n",
            "the typed letter replaced the selected match"
        );
    });
}

#[test]
fn a_click_on_the_find_box_gives_it_the_keyboard_back() {
    with_rig(&[("notes.md", "one two one\n")], |rig| {
        rig.press(u16::from(b'F'), Modifiers::CTRL);
        rig.type_text("two");
        assert_eq!(rig.selection().range(), (4..7).into());

        // A click into the text moves the keyboard to the editor ...
        let editor_at = Point::new(640.0, 400.0);
        rig.click(editor_at);
        rig.type_text("X");
        let find_text = rig
            .window()
            .view
            .borrow()
            .overlays()
            .find()
            .find_box()
            .text()
            .to_owned();
        assert_eq!(find_text, "two", "the editor, not the bar, took the letter");

        // ... and a click on the box brings it back to the bar.
        let find_at = rig.center_of(Role::Edit, "Find");
        rig.click(find_at);
        rig.press(u16::from(b'A'), Modifiers::CTRL);
        rig.type_text("one");
        assert_eq!(
            rig.matches(),
            2,
            "{:?}",
            rig.window().view.borrow().overlays()
        );
    });
}

#[test]
fn typing_into_the_folder_search_and_pressing_enter_lists_the_matching_files() {
    let files = [
        ("a.md", "alpha needle\n"),
        ("sub/b.md", "beta\nneedle again\n"),
        ("c.md", "nothing here\n"),
    ];
    with_rig(&files, |rig| {
        let window = rig.window();
        rig.controller.show_workspace(&window, &rig.work);
        rig.controller.refresh_all();

        rig.press(u16::from(b'F'), Modifiers::CTRL | Modifiers::SHIFT);
        assert!(window.view.borrow().overlays().folder_search_visible());
        rig.type_text("needle");
        rig.press(vk::RETURN, Modifiers::NONE);
        rig.settle();
        assert_eq!(rig.found(), ["a.md", "sub/b.md"]);

        // The panel draws the results; clicking a match opens its file at the line.
        let canvas = rig.paint();
        assert!(
            canvas
                .texts()
                .iter()
                .any(|text| text.contains("needle again")),
            "{:?}",
            canvas.texts()
        );
        let row = rig.center_of(Role::ListItem, "needle again");
        rig.click(row);
        rig.settle();
        let view = window.view.borrow();
        let tab = view.active_tab().unwrap();
        assert!(tab.entry.path().unwrap().ends_with("b.md"));
        let selection = tab.entry.document().borrow().primary_selection().range();
        assert_eq!(selection, (5..11).into());
        assert!(
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::Text { text, .. } if text == "sub/b.md")),
            "the file header is drawn"
        );
    });
}
