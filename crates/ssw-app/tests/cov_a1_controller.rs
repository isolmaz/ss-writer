//! The window, tab and command logic of the controller (`ui::controller`): title bar events,
//! tab and window commands, launches, startup plans, zoom, links, closing with unsaved changes,
//! the find / folder search commands and the folder search job. The real controller runs over a
//! started application in a temporary profile; dialogs and the shell are scripted. Nothing is
//! shown.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::{Args, Launch, LaunchItem};
use ssw_app::commands::Dispatch;
use ssw_app::documents::replace_text;
use ssw_app::i18n::localizer_for;
use ssw_app::session::{PlannedDocument, PlannedTab, PlannedWindow, StartupPlan};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::{CloseOutcome, Controller, Request, WindowCell, search_job};
use ssw_app::ui::overlays::OverlayAction;
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices, ShellRecord};
use ssw_app::ui::shell_view::ShellAction;
use ssw_app::workers::WindowId;
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::i18n::Arg;
use ssw_io::folder_search::SearchOptions;
use ssw_io::session::WindowPlacement;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::{EditorEvent, LinkTarget};
use ssw_ui::geometry::{Point, Size};
use ssw_ui::services::{
    Clipboard, Clock, Dialogs, FakeClock, MemoryClipboard, MessageBoxOptions, MessageBoxResult,
    OpenFileOptions, PickFolderOptions, SaveFileOptions, ServiceError,
};
use ssw_ui::shell::folder_search::{FolderSearchHit, FolderSearchRequest};
use ssw_ui::shell::layout::ShellMode;
use ssw_ui::shell::sidebar::SidebarEvent;
use ssw_ui::shell::tabs::{TabMenu, TabsEvent};
use ssw_ui::shell::titlebar::{FormatToggle, MenuKind, TitleEvent};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

/// Autosave would write the unsaved edits some tests keep.
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;

/// The files of the work folder; `notes.md` is the first window's document.
const FILES: &[(&str, &str)] = &[
    ("notes.md", "# Notes\n"),
    ("a.md", "# A\nfind the needle\n"),
    ("b.md", "# B\n"),
    ("c.markdown", "c\n"),
    ("Upper.MD", "u\n"),
    ("d.md", "été\n"),
    ("pic.png", "png"),
    ("sub/inner.md", "inner needle\n"),
];

fn process(args: &Args) -> ProcessEnv {
    ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

/// What a message box does before the scripted user answers it.
type Hook = Rc<RefCell<Option<Box<dyn FnMut()>>>>;

/// The scripted dialogs; a message box first runs the hook (once), like a window message that
/// arrives while the question is open.
struct HookedDialogs {
    inner: Box<dyn Dialogs>,
    hook: Hook,
}

impl Dialogs for HookedDialogs {
    fn open_file(
        &mut self,
        options: &OpenFileOptions,
    ) -> Result<Option<Vec<PathBuf>>, ServiceError> {
        self.inner.open_file(options)
    }

    fn save_file(&mut self, options: &SaveFileOptions) -> Result<Option<PathBuf>, ServiceError> {
        self.inner.save_file(options)
    }

    fn pick_folder(
        &mut self,
        options: &PickFolderOptions,
    ) -> Result<Option<PathBuf>, ServiceError> {
        self.inner.pick_folder(options)
    }

    fn message_box(
        &mut self,
        options: &MessageBoxOptions,
    ) -> Result<MessageBoxResult, ServiceError> {
        let hook = self.hook.borrow_mut().take();
        if let Some(mut hook) = hook {
            hook();
        }
        self.inner.message_box(options)
    }
}

struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
    clipboard: Rc<RefCell<MemoryClipboard>>,
    hook: Hook,
    work: PathBuf,
}

struct Host<B> {
    body: Option<B>,
    work: PathBuf,
}

impl<B: FnOnce(&Rig)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = ScriptedServices::new();
        let memory = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clipboard: SharedClipboard = memory.clone();
        let time: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let hook: Hook = Rc::default();
        let dialogs = Box::new(HookedDialogs {
            inner: services.dialogs(),
            hook: Rc::clone(&hook),
        });
        let plan = core.plan.clone();
        let controller = Controller::new(core, dialogs, services.shell(), clipboard, time);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services,
            clipboard: memory,
            hook,
            work: self.work.clone(),
        };
        (self.body.take().expect("the body runs once"))(&rig);
        assert_eq!(
            rig.services.take_unexpected(),
            None,
            "a dialog nobody expected"
        );
        assert!(
            rig.services.unused_answers().is_empty(),
            "an answer no dialog asked for"
        );
        0
    }
}

/// Runs `body` over a started application whose first window shows `notes.md` of `FILES`;
/// `settings` is the content of `settings.json`.
fn with_rig(settings: &str, body: impl FnOnce(&Rig)) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(work.join("sub")).unwrap();
    for (name, text) in FILES {
        std::fs::write(work.join(name), text).unwrap();
    }
    let roaming = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), settings).unwrap();
    let mut args = Args {
        paths: vec![work.join("notes.md")],
        ..Args::default()
    };
    args.test.hidden = true;
    args.test.profile = Some(tmp.path().join("profile"));
    let mut startup_dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process(&args),
        &mut *startup_dialogs,
        None,
        &mut Host {
            body: Some(body),
            work,
        },
    );
    assert_eq!(code, 0);
}

fn text_of(id: &str) -> String {
    localizer_for("en").text(id)
}

fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions).unwrap();
}

/// The file names of the tabs of `cell`; `None` for an untitled buffer.
fn names(cell: &WindowCell) -> Vec<Option<String>> {
    cell.view
        .borrow()
        .tabs()
        .iter()
        .map(|tab| {
            tab.entry
                .path()
                .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        })
        .collect()
}

fn file(name: &str) -> Option<String> {
    Some(name.to_owned())
}

fn keys(cell: &WindowCell) -> Vec<u64> {
    cell.view
        .borrow()
        .tabs()
        .iter()
        .map(|tab| tab.key)
        .collect()
}

fn active_key(cell: &WindowCell) -> u64 {
    cell.view.borrow().active_tab().expect("a tab is open").key
}

fn handled(outcome: &Dispatch) -> bool {
    matches!(outcome, Dispatch::Handled)
}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn id(&self) -> WindowId {
        self.window().id
    }

    fn cmd(&self, command: CommandId) -> Dispatch {
        self.controller.run_command(self.id(), command)
    }

    fn cmd_in(&self, cell: &WindowCell, command: CommandId) -> Dispatch {
        self.controller.run_command(cell.id, command)
    }

    fn act(&self, action: ShellAction) {
        self.controller.handle_action(self.id(), action);
    }

    fn open(&self, name: &str) {
        self.controller
            .open_file(&self.window(), &self.work.join(name))
            .unwrap();
        self.controller.refresh_all();
    }

    fn entry(&self) -> Rc<ssw_app::documents::DocEntry> {
        let window = self.window();
        let view = window.view.borrow();
        Rc::clone(&view.active_tab().expect("a tab is open").entry)
    }

    fn text(&self) -> String {
        self.entry().document().borrow().text()
    }

    fn set_text(&self, text: &str) {
        let changed = replace_text(&mut self.entry().document().borrow_mut(), text);
        assert!(changed, "the text differs");
        self.controller.refresh_all();
    }

    fn select(&self, selection: Selection) {
        self.window()
            .view
            .borrow_mut()
            .active_editor_mut()
            .expect("an editor")
            .go_to(selection);
    }

    fn answer(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }

    fn ok(&self) {
        self.answer(MessageBoxResult::Ok);
    }

    fn messages(&self) -> Vec<String> {
        self.services
            .dialog_log()
            .into_iter()
            .map(|dialog| dialog.message)
            .collect()
    }

    fn requests(&self) -> Vec<Request> {
        std::iter::from_fn(|| self.controller.take_request()).collect()
    }

    fn zoom(&self) -> u32 {
        self.controller.settings().appearance.zoom_percent
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

    /// A window without any tab.
    fn empty_window(&self) -> Rc<WindowCell> {
        self.controller
            .create_window(None, Size::new(800.0, 600.0), 1.0)
    }

    /// The matches the folder search panel of the first window shows.
    fn found(&self) -> Vec<String> {
        self.window()
            .view
            .borrow()
            .overlays()
            .folder_search()
            .files()
            .iter()
            .map(|file| file.path.clone())
            .collect()
    }

    fn search(&self, pattern: &str, regex: bool) {
        self.act(ShellAction::Overlay(OverlayAction::FolderSearch(
            FolderSearchRequest {
                pattern: pattern.to_owned(),
                case_sensitive: false,
                whole_word: false,
                regex,
            },
        )));
    }
}

// ----- debug output, active window, unknown windows --------------------------------------------

#[test]
fn windows_and_controller_describe_themselves() {
    with_rig(NO_AUTOSAVE, |rig| {
        let text = format!("{:?}", rig.window());
        assert!(
            text.starts_with("WindowCell { id: WindowId(1), placement: "),
            "{text}"
        );
        assert!(
            text.contains("width: 1280") && text.ends_with(", .. }"),
            "{text}"
        );
        assert_eq!(
            format!("{:?}", rig.controller),
            "Controller { windows: 1, active_window: Some(WindowId(1)), .. }"
        );
    });
}

#[test]
fn the_active_window_falls_back_to_the_last_one_and_unknown_windows_are_ignored() {
    with_rig(NO_AUTOSAVE, |rig| {
        let first = rig.window();
        assert!(handled(&rig.cmd(CommandId::FileNewWindow)));
        let second = Rc::clone(&rig.controller.windows()[1]);
        assert_eq!(rig.controller.active_window().unwrap().id, second.id);
        rig.controller.set_active_window(WindowId(999));
        assert_eq!(rig.controller.active_window().unwrap().id, second.id);
        rig.controller.set_active_window(first.id);
        assert_eq!(rig.controller.active_window().unwrap().id, first.id);

        // Nothing happens for a window that does not exist.
        let unknown = WindowId(999);
        assert!(matches!(
            rig.controller.run_command(unknown, CommandId::FileNew),
            Dispatch::Unhandled
        ));
        rig.controller.handle_action(
            unknown,
            ShellAction::Title(TitleEvent::Tabs(TabsEvent::NewTab)),
        );
        rig.controller.close_window(unknown);
        assert!(rig.controller.request_close_window(unknown));
        assert_eq!(rig.controller.windows().len(), 2);
        assert_eq!(names(&first), [file("notes.md")]);

        // Closing the active window activates the remaining one; closing it too leaves none.
        rig.controller.close_window(first.id);
        assert_eq!(rig.controller.active_window().unwrap().id, second.id);
        rig.controller.close_window(second.id);
        assert!(rig.controller.active_window().is_none());
        assert!(rig.controller.windows().is_empty());
    });
}

#[test]
fn the_plain_editor_menu_is_left_to_the_loop_owner() {
    with_rig(NO_AUTOSAVE, |rig| {
        let key = active_key(&rig.window());
        rig.requests();
        rig.act(ShellAction::EditorContext {
            key,
            at: Point::new(3.0, 4.0),
        });
        assert_eq!(names(&rig.window()), [file("notes.md")]);
        assert!(rig.controller.take_request().is_none());
        assert!(rig.services.dialog_log().is_empty());
    });
}

// ----- title bar events ------------------------------------------------------------------------

#[test]
fn the_title_bar_toggles_the_panes_and_the_pill_formats_the_selection() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        assert_eq!(window.view.borrow().layout().mode(), ShellMode::EditorOnly);
        rig.act(ShellAction::Title(TitleEvent::ToggleSidebar));
        assert_eq!(window.view.borrow().layout().mode(), ShellMode::Normal);
        assert!(!window.view.borrow().layout().inspector_visible());
        rig.act(ShellAction::Title(TitleEvent::ToggleInspector));
        assert!(window.view.borrow().layout().inspector_visible());
        rig.act(ShellAction::Title(TitleEvent::ToggleInspector));
        assert!(!window.view.borrow().layout().inspector_visible());

        for (toggle, expected) in [
            (FormatToggle::Bold, "hello **world**"),
            (FormatToggle::Italic, "hello *world*"),
            (FormatToggle::Underline, "hello ~world~"),
        ] {
            rig.set_text("hello world");
            rig.select(Selection::new(6, 11));
            rig.act(ShellAction::Title(TitleEvent::Format(toggle)));
            assert_eq!(rig.text(), expected);
        }
    });
}

#[test]
fn the_tab_strip_events_close_add_and_reorder_tabs_and_menus_change_nothing() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.open("a.md");
        let window = rig.window();
        let ids = keys(&window);
        assert_eq!(names(&window), [file("notes.md"), file("a.md")]);

        rig.act(ShellAction::Title(TitleEvent::Tabs(TabsEvent::Reordered {
            key: ids[1],
            from: 1,
            to: 0,
        })));
        assert_eq!(names(&window), [file("a.md"), file("notes.md")]);
        assert_eq!(active_key(&window), ids[1], "the active tab moves along");

        rig.act(ShellAction::Title(TitleEvent::Tabs(
            TabsEvent::CloseRequested(ids[1]),
        )));
        assert_eq!(names(&window), [file("notes.md")]);
        rig.act(ShellAction::Title(TitleEvent::Tabs(TabsEvent::NewTab)));
        assert_eq!(names(&window), [file("notes.md"), None]);

        rig.requests();
        rig.act(ShellAction::Title(TitleEvent::MenuRequested {
            menu: MenuKind::File,
            anchor: Point::new(1.0, 2.0),
        }));
        rig.act(ShellAction::Title(TitleEvent::Tabs(
            TabsEvent::ContextMenu {
                key: ids[0],
                at: Point::new(1.0, 2.0),
                menu: TabMenu {
                    can_close_others: false,
                    can_close_to_right: false,
                    has_path: true,
                },
            },
        )));
        assert_eq!(names(&window), [file("notes.md"), None]);
        assert!(rig.controller.take_request().is_none());
    });
}

#[test]
fn a_dragged_out_tab_becomes_its_own_window() {
    with_rig(NO_AUTOSAVE, |rig| {
        let first = rig.window();
        let root = rig.work.clone();
        rig.controller.show_workspace(&first, &root);
        let only = active_key(&first);
        let torn = |key: u64| {
            ShellAction::Title(TitleEvent::Tabs(TabsEvent::TornOut {
                key,
                at: Point::new(0.0, 0.0),
            }))
        };

        // The only tab stays: a window needs one.
        rig.act(torn(only));
        assert_eq!(rig.controller.windows().len(), 1);

        rig.open("a.md");
        let a_key = active_key(&first);
        // A key that is not a tab of the window tears nothing out.
        rig.act(torn(9999));
        assert_eq!(rig.controller.windows().len(), 1);
        assert_eq!(names(&first), [file("notes.md"), file("a.md")]);

        rig.requests();
        rig.act(torn(a_key));
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 2);
        let fresh = &windows[1];
        assert_eq!(names(&first), [file("notes.md")]);
        assert_eq!(names(fresh), [file("a.md")]);
        assert_eq!(
            fresh.view.borrow().tabs()[0].workspace.as_deref(),
            Some(root.as_path())
        );
        assert_eq!(
            fresh.view.borrow().layout().size(),
            first.view.borrow().layout().size()
        );
        assert_eq!(rig.controller.active_window().unwrap().id, fresh.id);
        assert!(rig.requests().contains(&Request::NewWindow(fresh.id)));
    });
}

// ----- windows: new window, merge, launches ----------------------------------------------------

#[test]
fn merging_moves_the_tabs_of_every_other_window_into_the_active_one() {
    with_rig(NO_AUTOSAVE, |rig| {
        let first = rig.window();
        assert!(handled(&rig.cmd(CommandId::FileNewWindow)));
        let second = Rc::clone(&rig.controller.windows()[1]);
        assert_eq!(
            names(&second),
            [None],
            "the new window starts with an untitled tab"
        );
        assert_eq!(
            second.view.borrow().layout().size(),
            first.view.borrow().layout().size()
        );
        rig.controller.open_launch(&Launch {
            items: vec![LaunchItem::File(rig.work.join("a.md"))],
            new_window: true,
        });
        let third = Rc::clone(&rig.controller.windows()[2]);
        assert_eq!(names(&third), [file("a.md")]);
        assert_eq!(rig.controller.active_window().unwrap().id, third.id);

        rig.requests();
        assert!(handled(&rig.cmd_in(&first, CommandId::WindowMergeAll)));
        assert_eq!(rig.controller.windows().len(), 1);
        assert_eq!(names(&first), [file("notes.md"), None, file("a.md")]);
        let requests = rig.requests();
        assert!(requests.contains(&Request::CloseWindow(second.id)));
        assert!(requests.contains(&Request::CloseWindow(third.id)));
        assert_eq!(rig.controller.active_window().unwrap().id, first.id);
    });
}

#[test]
fn a_launch_adds_tabs_to_the_active_window_and_gives_folders_their_own() {
    with_rig(NO_AUTOSAVE, |rig| {
        let first = rig.window();
        let folder = rig.work.join("sub");
        rig.ok();
        rig.controller.open_launch(&Launch {
            items: vec![
                LaunchItem::File(rig.work.join("a.md")),
                LaunchItem::Folder(folder.clone()),
                LaunchItem::File(rig.work.join("missing.md")),
            ],
            new_window: false,
        });
        assert_eq!(names(&first), [file("notes.md"), file("a.md")]);
        assert_eq!(rig.messages(), [text_of("app-error-not-found")]);
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 2);
        let fresh = &windows[1];
        assert_eq!(names(fresh), [None]);
        assert!(fresh.tree.borrow().is_some());
        assert_eq!(
            fresh.view.borrow().tabs()[0].workspace.as_deref(),
            Some(folder.as_path())
        );
        assert_eq!(rig.controller.active_window().unwrap().id, fresh.id);
    });
}

#[test]
fn a_new_window_launch_opens_a_window_even_without_files() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.controller.open_launch(&Launch {
            items: vec![LaunchItem::File(rig.work.join("b.md"))],
            new_window: true,
        });
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 2);
        assert_eq!(names(&windows[1]), [file("b.md")]);
        assert_eq!(names(&windows[0]), [file("notes.md")]);

        rig.controller.open_launch(&Launch {
            items: Vec::new(),
            new_window: true,
        });
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 3);
        assert_eq!(rig.controller.active_window().unwrap().id, windows[2].id);
    });
}

#[test]
fn a_launch_without_any_window_opens_one_only_for_files() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.controller.close_window(rig.id());
        assert!(rig.controller.windows().is_empty());
        rig.controller.open_launch(&Launch {
            items: vec![LaunchItem::Folder(rig.work.join("sub"))],
            new_window: false,
        });
        assert!(
            rig.controller.windows().is_empty(),
            "a folder alone has no window to join"
        );
        rig.controller.open_launch(&Launch {
            items: vec![LaunchItem::File(rig.work.join("a.md"))],
            new_window: false,
        });
        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(names(&windows[0]), [file("a.md")]);
        assert_eq!(rig.controller.active_window().unwrap().id, windows[0].id);
    });
}

// ----- startup plans ---------------------------------------------------------------------------

#[test]
fn a_startup_plan_restores_tabs_skips_what_is_gone_and_picks_the_active_window() {
    with_rig(NO_AUTOSAVE, |rig| {
        let sub = rig.work.join("sub");
        rig.controller
            .docs()
            .env()
            .store
            .borrow()
            .untitled_put("u-1", "restored draft\n", 10)
            .unwrap();
        let tab = |document: PlannedDocument, workspace: Option<PathBuf>| PlannedTab {
            document,
            workspace,
        };
        let plan = StartupPlan {
            windows: vec![
                PlannedWindow {
                    placement: Some(WindowPlacement {
                        x: 5,
                        y: 6,
                        width: 700,
                        height: 500,
                        maximized: false,
                    }),
                    tabs: vec![
                        tab(PlannedDocument::File(rig.work.join("missing.md")), None),
                        tab(PlannedDocument::RestoredUntitled("u-1".to_owned()), None),
                        tab(PlannedDocument::RestoredUntitled("u-gone".to_owned()), None),
                        tab(
                            PlannedDocument::File(rig.work.join("a.md")),
                            Some(sub.clone()),
                        ),
                    ],
                    active_tab: Some(3),
                },
                PlannedWindow {
                    placement: None,
                    tabs: Vec::new(),
                    active_tab: None,
                },
                PlannedWindow {
                    placement: None,
                    tabs: vec![tab(PlannedDocument::File(rig.work.join("b.md")), None)],
                    active_tab: None,
                },
                PlannedWindow {
                    placement: None,
                    tabs: vec![
                        tab(PlannedDocument::NewUntitled, None),
                        tab(PlannedDocument::NewUntitled, None),
                    ],
                    active_tab: Some(0),
                },
            ],
            active_window: Some(2),
        };
        rig.ok();
        rig.controller.open_plan(&plan);
        assert_eq!(rig.messages(), [text_of("app-error-not-found")]);

        let windows = rig.controller.windows();
        assert_eq!(windows.len(), 5);
        let restored = &windows[1];
        assert_eq!(restored.placement.get().width, 700);
        assert_eq!(names(restored), [None, file("a.md")]);
        assert_eq!(
            restored.view.borrow().tabs()[0]
                .entry
                .document()
                .borrow()
                .text(),
            "restored draft\n"
        );
        assert_eq!(active_key(restored), keys(restored)[1]);
        assert!(
            restored.tree.borrow().is_some(),
            "the tab's workspace is shown"
        );

        assert_eq!(
            names(&windows[2]),
            [None],
            "an empty window gets an untitled tab"
        );
        assert_eq!(names(&windows[3]), [file("b.md")]);
        assert_eq!(
            windows[3].view.borrow().layout().mode(),
            ShellMode::EditorOnly,
            "a single file opens editor-only"
        );
        assert_eq!(names(&windows[4]), [None, None]);
        assert_eq!(windows[4].view.borrow().layout().mode(), ShellMode::Normal);
        assert_eq!(rig.controller.active_window().unwrap().id, windows[3].id);

        // A plan without an active window leaves the active window alone.
        rig.controller.open_plan(&StartupPlan::default());
        rig.controller.open_plan(&StartupPlan {
            windows: Vec::new(),
            active_window: Some(7),
        });
        assert_eq!(rig.controller.active_window().unwrap().id, windows[3].id);
        assert_eq!(rig.controller.windows().len(), 5);
    });
}

// ----- zoom ------------------------------------------------------------------------------------

#[test]
fn the_zoom_is_clamped_and_follows_every_window_and_the_zoom_commands() {
    with_rig(NO_AUTOSAVE, |rig| {
        assert!(handled(&rig.cmd(CommandId::FileNewWindow)));
        let windows = rig.controller.windows();
        let key = active_key(&windows[0]);
        rig.act(ShellAction::Editor(key, EditorEvent::ZoomChanged(500)));
        assert_eq!(rig.zoom(), 400);
        for window in &windows {
            assert_eq!(window.view.borrow().tabs()[0].editor.zoom_percent(), 400);
        }
        rig.controller.set_zoom(10);
        assert_eq!(rig.zoom(), 50);
        for window in &windows {
            assert_eq!(window.view.borrow().tabs()[0].editor.zoom_percent(), 50);
        }
        assert!(handled(&rig.cmd(CommandId::ViewZoomReset)));
        assert_eq!(rig.zoom(), 100);
        assert!(handled(&rig.cmd(CommandId::ViewZoomIn)));
        assert_eq!(rig.zoom(), 110);
        assert!(handled(&rig.cmd(CommandId::ViewZoomOut)));
        assert!(handled(&rig.cmd(CommandId::ViewZoomOut)));
        assert_eq!(rig.zoom(), 90);
        assert_eq!(windows[1].view.borrow().tabs()[0].editor.zoom_percent(), 90);
    });
}

// ----- links in the editor ---------------------------------------------------------------------

#[test]
fn links_open_the_browser_or_a_markdown_file_next_to_the_document() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        let key = active_key(&window);
        let link = |url: &str| {
            ShellAction::Editor(
                key,
                EditorEvent::LinkActivated(LinkTarget::Url(url.to_owned())),
            )
        };
        rig.act(link("https://example.com/a"));
        rig.act(link("http://example.com/b"));
        assert_eq!(
            rig.services.shell_log(),
            [
                ShellRecord::OpenUrl("https://example.com/a".to_owned()),
                ShellRecord::OpenUrl("http://example.com/b".to_owned())
            ]
        );
        assert_eq!(names(&window), [file("notes.md")]);

        rig.act(link("a.md"));
        assert_eq!(names(&window), [file("notes.md"), file("a.md")]);
        rig.act(link("c.markdown"));
        rig.act(link("Upper.MD"));
        assert_eq!(
            names(&window),
            [
                file("notes.md"),
                file("a.md"),
                file("c.markdown"),
                file("Upper.MD")
            ]
        );
        // Other files and names without an extension are no documents to follow.
        for other in ["pic.png", "readme", "notes.txt"] {
            rig.act(link(other));
        }
        assert_eq!(names(&window).len(), 4);
        assert_eq!(rig.services.shell_log().len(), 2);

        // An untitled document has no folder to resolve the link in.
        assert!(handled(&rig.cmd(CommandId::TabNew)));
        rig.act(link("b.md"));
        assert_eq!(names(&window).len(), 5);
    });
}

// ----- unsaved changes, closing tabs and windows ------------------------------------------------

#[test]
fn a_failing_autosave_is_reported_when_switching_and_keeps_the_tab_open() {
    with_rig("{}", |rig| {
        rig.open("a.md");
        let window = rig.window();
        let ids = keys(&window);
        let a_path = rig.work.join("a.md");
        // The file turns read-only behind the document's back.
        set_readonly(&a_path, true);
        rig.set_text("changed\n");

        rig.ok();
        rig.act(ShellAction::Title(TitleEvent::Tabs(TabsEvent::Activated(
            ids[0],
        ))));
        assert_eq!(rig.messages(), [text_of("app-error-read-only")]);
        assert_eq!(active_key(&window), ids[0]);

        rig.ok();
        assert_eq!(
            rig.controller.close_tab(&window, ids[1]),
            CloseOutcome::Cancelled
        );
        assert_eq!(names(&window), [file("notes.md"), file("a.md")]);
        assert_eq!(rig.messages().len(), 2);
        set_readonly(&a_path, false);
    });
}

#[test]
fn closing_a_tab_with_unsaved_changes_asks_and_saves_or_discards() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        let key = active_key(&window);
        let notes = rig.work.join("notes.md");
        assert_eq!(
            rig.controller.close_tab(&window, 9999),
            CloseOutcome::Closed
        );
        assert_eq!(names(&window), [file("notes.md")]);

        rig.set_text("edited\n");
        rig.answer(MessageBoxResult::Cancel);
        assert_eq!(
            rig.controller.close_tab(&window, key),
            CloseOutcome::Cancelled
        );
        assert_eq!(names(&window), [file("notes.md")]);

        // "Save" fails: the error is shown and the tab stays.
        set_readonly(&notes, true);
        rig.answer(MessageBoxResult::Save);
        rig.ok();
        assert_eq!(
            rig.controller.close_tab(&window, key),
            CloseOutcome::Cancelled
        );
        assert_eq!(
            rig.messages().last().unwrap(),
            &text_of("app-error-read-only")
        );
        set_readonly(&notes, false);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "# Notes\n");

        // "Save" works: the file is written and the last tab is replaced by an empty one.
        rig.answer(MessageBoxResult::Save);
        assert_eq!(rig.controller.close_tab(&window, key), CloseOutcome::Closed);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "edited\n");
        assert_eq!(names(&window), [None]);

        // "Save" of an untitled document asks for a file; cancelling that keeps the tab.
        rig.set_text("draft\n");
        let untitled = active_key(&window);
        rig.answer(MessageBoxResult::Save);
        rig.services.answer(DialogKind::SaveFile, Answer::Cancel);
        assert_eq!(
            rig.controller.close_tab(&window, untitled),
            CloseOutcome::Cancelled
        );
        rig.answer(MessageBoxResult::DontSave);
        assert_eq!(
            rig.controller.close_tab(&window, untitled),
            CloseOutcome::Closed
        );
        assert_eq!(names(&window), [None]);
        assert_eq!(rig.text(), "");
    });
}

#[test]
fn a_tab_that_vanishes_while_the_question_is_open_is_just_closed() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.open("a.md");
        let window = rig.window();
        let key = active_key(&window);
        rig.set_text("edited\n");
        let target = Rc::clone(&window);
        *rig.hook.borrow_mut() = Some(Box::new(move || {
            target.view.borrow_mut().close_tab(key);
        }));
        rig.answer(MessageBoxResult::DontSave);
        assert_eq!(rig.controller.close_tab(&window, key), CloseOutcome::Closed);
        assert_eq!(names(&window), [file("notes.md")]);
    });
}

#[test]
fn the_last_tab_of_a_second_window_closes_that_window() {
    with_rig(NO_AUTOSAVE, |rig| {
        assert!(handled(&rig.cmd(CommandId::FileNewWindow)));
        let second = Rc::clone(&rig.controller.windows()[1]);
        let key = active_key(&second);
        rig.requests();
        assert_eq!(rig.controller.close_tab(&second, key), CloseOutcome::Closed);
        assert_eq!(rig.controller.windows().len(), 1);
        assert!(rig.requests().contains(&Request::CloseWindow(second.id)));
    });
}

#[test]
fn closing_a_window_asks_about_every_tab_until_one_is_cancelled() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        rig.set_text("notes edited\n");
        rig.open("a.md");
        rig.set_text("a edited\n");
        let id = window.id;

        rig.answer(MessageBoxResult::DontSave);
        rig.answer(MessageBoxResult::Cancel);
        assert!(!rig.controller.request_close_window(id));
        assert_eq!(names(&window).len(), 2);

        rig.answer(MessageBoxResult::DontSave);
        rig.answer(MessageBoxResult::DontSave);
        assert!(rig.controller.request_close_window(id));
    });
}

// ----- tab commands ----------------------------------------------------------------------------

#[test]
fn tab_commands_cycle_close_and_reopen_tabs_and_copy_or_reveal_the_path() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        // One tab: nothing to switch to.
        assert!(handled(&rig.cmd(CommandId::TabNext)));
        assert_eq!(names(&window), [file("notes.md")]);
        assert!(handled(&rig.cmd(CommandId::TabReopenClosed)));
        assert_eq!(names(&window).len(), 1, "nothing was closed yet");

        rig.open("a.md");
        rig.open("b.md");
        let ids = keys(&window);
        assert_eq!(active_key(&window), ids[2]);
        assert!(handled(&rig.cmd(CommandId::TabNext)));
        assert_eq!(active_key(&window), ids[0], "wraps to the first tab");
        assert!(handled(&rig.cmd(CommandId::TabPrevious)));
        assert_eq!(active_key(&window), ids[2], "wraps to the last tab");
        assert!(handled(&rig.cmd(CommandId::TabPrevious)));
        assert_eq!(active_key(&window), ids[1]);

        let a_path = rig.work.join("a.md");
        assert!(handled(&rig.cmd(CommandId::TabCopyPath)));
        let copied = rig
            .clipboard
            .borrow()
            .text()
            .unwrap()
            .expect("a path was copied");
        assert!(copied.ends_with("a.md"), "{copied}");
        assert!(handled(&rig.cmd(CommandId::TabRevealInExplorer)));
        let revealed = rig.services.shell_log();
        assert_eq!(revealed.len(), 1);
        assert!(
            matches!(&revealed[0], ShellRecord::Reveal(path) if path.ends_with("a.md")),
            "{revealed:?} for {}",
            a_path.display()
        );

        assert!(handled(&rig.cmd(CommandId::TabCloseToRight)));
        assert_eq!(names(&window), [file("notes.md"), file("a.md")]);
        assert!(handled(&rig.cmd(CommandId::TabReopenClosed)));
        assert_eq!(
            names(&window),
            [file("notes.md"), file("a.md"), file("b.md")]
        );
        let b_key = active_key(&window);
        assert_eq!(b_key, *keys(&window).last().unwrap());

        assert!(handled(&rig.cmd(CommandId::TabCloseOthers)));
        assert_eq!(names(&window), [file("b.md")]);
        for expected in [2, 3, 3] {
            assert!(handled(&rig.cmd(CommandId::TabReopenClosed)));
            assert_eq!(names(&window).len(), expected);
        }

        // An untitled document has no path to copy or reveal.
        assert!(handled(&rig.cmd(CommandId::TabNew)));
        assert!(handled(&rig.cmd(CommandId::TabCopyPath)));
        assert!(handled(&rig.cmd(CommandId::TabRevealInExplorer)));
        assert_eq!(rig.clipboard.borrow().text().unwrap().unwrap(), copied);
        assert_eq!(rig.services.shell_log().len(), 1);
    });
}

#[test]
fn a_window_without_tabs_ignores_tab_commands() {
    with_rig(NO_AUTOSAVE, |rig| {
        let empty = rig.empty_window();
        for command in [
            CommandId::TabClose,
            CommandId::TabCloseOthers,
            CommandId::TabCloseToRight,
            CommandId::TabNext,
            CommandId::TabCopyPath,
            CommandId::TabRevealInExplorer,
        ] {
            assert!(handled(&rig.cmd_in(&empty, command)), "{command}");
        }
        rig.controller.handle_action(
            empty.id,
            ShellAction::Title(TitleEvent::Tabs(TabsEvent::Activated(5))),
        );
        assert!(empty.view.borrow().tabs().is_empty());
        assert_eq!(rig.controller.windows().len(), 2);
        assert!(rig.services.shell_log().is_empty());
        assert!(rig.clipboard.borrow().text().unwrap().is_none());
    });
}

// ----- the workspace of a window ---------------------------------------------------------------

#[test]
fn showing_a_workspace_follows_the_sidebar_state_and_reports_missing_folders() {
    with_rig(NO_AUTOSAVE, |rig| {
        let first = rig.window();
        let root = rig.work.clone();
        // Editor-only mode returns to the normal one.
        rig.controller.show_workspace(&first, &root);
        assert_eq!(first.view.borrow().layout().mode(), ShellMode::Normal);
        assert!(first.view.borrow().layout().sidebar_visible());
        assert_eq!(
            first.view.borrow().tabs()[0].workspace.as_deref(),
            Some(root.as_path())
        );
        // A hidden sidebar is shown, a shown one stays.
        first
            .view
            .borrow_mut()
            .layout_mut()
            .set_sidebar_visible(false);
        rig.controller.show_workspace(&first, &root);
        assert!(first.view.borrow().layout().sidebar_visible());
        rig.controller.show_workspace(&first, &root);
        assert!(first.view.borrow().layout().sidebar_visible());

        // A window without a tab has no tab to remember the folder.
        let empty = rig.empty_window();
        rig.controller.show_workspace(&empty, &root);
        assert!(empty.tree.borrow().is_some());

        let missing = root.join("nowhere");
        let other = rig.empty_window();
        rig.ok();
        rig.controller.show_workspace(&other, &missing);
        assert!(other.tree.borrow().is_none());
        let log = rig.services.dialog_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].title, text_of("window-error-title"));
        assert_eq!(
            log[0].message,
            localizer_for("en").format(
                "window-folder-missing",
                &[("path", Arg::Str(&missing.to_string_lossy()))]
            )
        );
    });
}

#[test]
fn the_sidebar_opens_files_and_ignores_other_events() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        let root = rig.work.clone();
        rig.controller.show_workspace(&window, &root);
        window.view.borrow_mut().paint(
            &mut RecordingCanvas::new(),
            &MonoTextEngine::default(),
            false,
        );
        let (b_key, png_key) = {
            let view = window.view.borrow();
            let sidebar = view.sidebar().expect("a sidebar");
            let rows = sidebar.row_names();
            let key = |name: &str| {
                let index = rows.iter().position(|row| row == name).expect(name);
                sidebar.row_key(index).unwrap()
            };
            // The tree hides the extensions by default.
            (key("b"), key("pic"))
        };
        rig.act(ShellAction::Sidebar(SidebarEvent::Selected(b_key)));
        rig.act(ShellAction::Sidebar(SidebarEvent::Open {
            key: 99_999,
            new_tab: false,
        }));
        assert_eq!(names(&window), [file("notes.md")]);
        rig.act(ShellAction::Sidebar(SidebarEvent::Open {
            key: b_key,
            new_tab: false,
        }));
        assert_eq!(names(&window), [file("notes.md"), file("b.md")]);
        rig.act(ShellAction::Sidebar(SidebarEvent::Open {
            key: png_key,
            new_tab: true,
        }));
        let log = rig.services.shell_log();
        assert_eq!(log.len(), 1);
        assert!(
            matches!(&log[0], ShellRecord::OpenPath(path) if path.ends_with("pic.png")),
            "{log:?}"
        );
        assert_eq!(names(&window).len(), 2);
    });
}

// ----- view commands ---------------------------------------------------------------------------

#[test]
fn view_commands_toggle_the_panes_and_modes() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        // A window starts with the sidebar hidden. In the editor-only window of a single file
        // the first toggle shows the folder of the file, the second hides the sidebar, the
        // third shows the folder that is already loaded.
        assert!(!window.view.borrow().layout().sidebar_visible());
        assert!(window.tree.borrow().is_none());
        assert!(handled(&rig.cmd(CommandId::ViewToggleSidebar)));
        assert_eq!(window.view.borrow().layout().mode(), ShellMode::Normal);
        assert!(window.view.borrow().layout().sidebar_visible());
        assert!(window.tree.borrow().is_some());
        let parent = rig.entry().path().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(
            window.view.borrow().tabs()[0].workspace.as_deref(),
            Some(parent.as_path())
        );
        assert!(handled(&rig.cmd(CommandId::ViewToggleSidebar)));
        assert!(!window.view.borrow().layout().sidebar_visible());
        assert!(handled(&rig.cmd(CommandId::ViewToggleSidebar)));
        assert!(window.view.borrow().layout().sidebar_visible());

        // An untitled document has no folder to show: the sidebar just toggles.
        assert!(handled(&rig.cmd(CommandId::FileNewWindow)));
        let second = Rc::clone(&rig.controller.windows()[1]);
        assert!(!second.view.borrow().layout().sidebar_visible());
        assert!(handled(&rig.cmd_in(&second, CommandId::ViewToggleSidebar)));
        assert!(second.view.borrow().layout().sidebar_visible());
        assert!(second.tree.borrow().is_none());
        assert!(handled(&rig.cmd_in(&second, CommandId::ViewToggleSidebar)));
        assert!(!second.view.borrow().layout().sidebar_visible());

        assert!(handled(&rig.cmd(CommandId::ViewToggleInspector)));
        assert!(window.view.borrow().layout().inspector_visible());
        assert!(handled(&rig.cmd(CommandId::ViewToggleStatusBar)));
        assert!(!window.view.borrow().layout().status_bar_visible());
        assert!(handled(&rig.cmd(CommandId::ViewToggleFormatPill)));
        assert!(!window.view.borrow().titlebar().pill_visible());

        for (command, mode) in [
            (CommandId::ViewZenMode, ShellMode::Zen),
            (CommandId::ViewFullScreen, ShellMode::FullScreen),
        ] {
            assert!(handled(&rig.cmd(command)));
            assert_eq!(window.view.borrow().layout().mode(), mode);
            assert!(handled(&rig.cmd(command)));
            assert_eq!(window.view.borrow().layout().mode(), ShellMode::Normal);
        }

        let pane = window.view.borrow().focused_pane();
        assert!(handled(&rig.cmd(CommandId::ViewNextPane)));
        assert_ne!(window.view.borrow().focused_pane(), pane);

        assert!(!window.view.borrow().tabs()[0].editor.typewriter());
        assert!(handled(&rig.cmd(CommandId::ViewTypewriterMode)));
        assert!(window.view.borrow().tabs()[0].editor.typewriter());
    });
}

#[test]
fn the_link_command_opens_the_link_popup_and_unhandled_commands_are_reported() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        assert!(!window.view.borrow().overlays().link_visible());
        assert!(handled(&rig.cmd(CommandId::FormatLink)));
        assert!(window.view.borrow().overlays().link_visible());
        assert!(matches!(
            rig.cmd(CommandId::SidebarRename),
            Dispatch::Unhandled
        ));
    });
}

#[test]
fn insert_date_puts_todays_long_date_at_the_caret() {
    with_rig(NO_AUTOSAVE, |rig| {
        let (year, month, day) = ssw_win::locale::local_today();
        let expected = ssw_win::locale::Locale::user_default()
            .format_date_long(year, month, day)
            .unwrap();
        rig.select(Selection::caret(8));
        assert!(handled(&rig.cmd(CommandId::EditInsertDate)));
        assert_eq!(rig.text(), format!("# Notes\n{expected}"));
        assert!(rig.entry().is_dirty());

        // A plain-text document takes the date as it is (no Markdown escape).
        std::fs::write(rig.work.join("plain.txt"), "1\n").unwrap();
        rig.open("plain.txt");
        assert!(handled(&rig.cmd(CommandId::EditInsertDate)));
        assert_eq!(rig.text(), format!("{expected}1\n"));

        // Nothing to insert into without a tab.
        let empty = rig.empty_window();
        assert!(matches!(
            rig.cmd_in(&empty, CommandId::EditInsertDate),
            Dispatch::Unhandled
        ));
    });
}

#[test]
fn format_image_asks_for_pictures_and_inserts_them_like_pasted_files() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        rig.select(Selection::caret(8));
        // Cancelling changes nothing.
        rig.services.answer(DialogKind::OpenFile, Answer::Cancel);
        assert!(handled(&rig.cmd(CommandId::FormatImage)));
        assert_eq!(rig.text(), "# Notes\n");
        let log = rig.services.dialog_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].title, text_of("clipboard-insert-image-title"));

        // A picture next to the document is linked where it is.
        rig.services.answer(
            DialogKind::OpenFile,
            Answer::Paths(vec![rig.work.join("pic.png")]),
        );
        assert!(handled(&rig.cmd(CommandId::FormatImage)));
        assert_eq!(rig.text(), "# Notes\n![](pic.png)");
        assert!(!rig.work.join("pic-1.png").exists());

        // A read-only editor and a plain-text document are left alone.
        window
            .view
            .borrow_mut()
            .active_editor_mut()
            .unwrap()
            .set_read_only(true);
        assert!(handled(&rig.cmd(CommandId::FormatImage)));
        assert_eq!(rig.text(), "# Notes\n![](pic.png)");
        std::fs::write(rig.work.join("plain.txt"), "1\n").unwrap();
        rig.open("plain.txt");
        assert!(handled(&rig.cmd(CommandId::FormatImage)));
        assert_eq!(rig.text(), "1\n");
        assert_eq!(rig.services.dialog_log().len(), 2);

        let empty = rig.empty_window();
        assert!(matches!(
            rig.cmd_in(&empty, CommandId::FormatImage),
            Dispatch::Unhandled
        ));
    });
}

#[test]
fn search_commands_open_the_find_bar_go_to_line_and_the_folder_search() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        let repainted = |command: CommandId| {
            window.dirty.set(false);
            assert!(handled(&rig.cmd(command)), "{command}");
            assert!(window.dirty.get(), "{command} asks for a repaint");
        };
        repainted(CommandId::SearchFind);
        assert!(window.view.borrow().overlays().find_visible());
        repainted(CommandId::SearchReplace);
        repainted(CommandId::SearchFindNext);
        repainted(CommandId::SearchFindPrevious);
        assert!(window.view.borrow().overlays().find_visible());
        assert!(!window.view.borrow().overlays().goto_visible());
        repainted(CommandId::SearchGoToLine);
        assert!(window.view.borrow().overlays().goto_visible());

        // F3 and Shift+F3 open the bar when it is closed.
        for command in [CommandId::SearchFindNext, CommandId::SearchFindPrevious] {
            assert!(handled(&rig.cmd(CommandId::FileNewWindow)));
            let fresh = Rc::clone(rig.controller.windows().last().unwrap());
            assert!(!fresh.view.borrow().overlays().find_visible());
            assert!(handled(&rig.cmd_in(&fresh, command)));
            assert!(fresh.view.borrow().overlays().find_visible());
        }

        // The selection seeds the folder search, which needs the sidebar.
        rig.select(Selection::new(2, 7));
        window
            .view
            .borrow_mut()
            .layout_mut()
            .set_sidebar_visible(false);
        repainted(CommandId::SearchInFolder);
        assert!(window.view.borrow().layout().sidebar_visible());
        assert!(window.view.borrow().overlays().folder_search_visible());
        assert_eq!(
            window
                .view
                .borrow()
                .overlays()
                .folder_search()
                .input()
                .text(),
            "Notes"
        );
        repainted(CommandId::SearchInFolder);
        assert!(window.view.borrow().layout().sidebar_visible());
    });
}

// ----- folder search and opening matches -------------------------------------------------------

#[test]
fn a_folder_search_runs_on_a_worker_and_a_newer_or_cleared_search_replaces_it() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        let root = rig.work.clone();
        rig.controller.show_workspace(&window, &root);
        rig.search("needle", false);
        rig.settle();
        let mut found = rig.found();
        found.sort();
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].ends_with("a.md"), "{found:?}");
        assert!(found[1].ends_with("inner.md"), "{found:?}");

        // An invalid pattern finds nothing and leaves the shown matches alone.
        rig.search("(", true);
        rig.settle();
        assert_eq!(rig.found().len(), 2);

        // A search that was cleared again before its result arrived shows nothing.
        rig.search("inner", false);
        rig.act(ShellAction::Overlay(OverlayAction::FolderSearchCleared));
        rig.settle();
        assert_eq!(rig.found().len(), 2);

        // Without a workspace there is nothing to search or open.
        let untitled = rig.empty_window();
        rig.controller.new_untitled(&untitled);
        rig.controller.handle_action(
            untitled.id,
            ShellAction::Overlay(OverlayAction::FolderSearch(FolderSearchRequest {
                pattern: "needle".to_owned(),
                case_sensitive: false,
                whole_word: false,
                regex: false,
            })),
        );
        rig.controller.handle_action(
            untitled.id,
            ShellAction::Overlay(OverlayAction::OpenFile("a.md".to_owned())),
        );
        rig.controller.handle_action(
            untitled.id,
            ShellAction::Overlay(OverlayAction::OpenMatch(FolderSearchHit {
                path: "a.md".to_owned(),
                line: 1,
                range: 0..1,
            })),
        );
        assert_eq!(rig.controller.core().borrow().workers.pending(), 0);
        assert_eq!(names(&untitled), [None]);
    });
}

#[test]
fn the_search_job_collects_matches_and_stops_when_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("one.md"), "needle\n").unwrap();
    std::fs::write(dir.path().join("two.md"), "needle again\n").unwrap();
    let options = SearchOptions::literal("needle");

    let (files, summary) = search_job(dir.path(), &options, &|| false).unwrap();
    assert_eq!(files.len(), 2);
    assert!(!summary.cancelled);

    let (files, summary) = search_job(dir.path(), &options, &|| true).unwrap();
    assert_eq!(files.len(), 1, "the search stops after the first file");
    assert!(summary.cancelled);

    let invalid = SearchOptions {
        regex: true,
        ..SearchOptions::literal("(")
    };
    assert!(search_job(dir.path(), &invalid, &|| false).is_none());
    assert!(search_job(dir.path(), &SearchOptions::literal(""), &|| false).is_none());
}

#[test]
fn overlay_actions_open_files_and_select_the_match() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        let root = rig.work.clone();
        rig.controller.show_workspace(&window, &root);
        let selection = |rig: &Rig| {
            let entry = rig.entry();
            let document = entry.document().borrow();
            let selection = document.primary_selection();
            (selection.anchor, selection.head)
        };
        let open_match = |path: &str, line: u64, range: std::ops::Range<usize>| {
            rig.act(ShellAction::Overlay(OverlayAction::OpenMatch(
                FolderSearchHit {
                    path: path.to_owned(),
                    line,
                    range,
                },
            )));
        };

        // "# A\n" is 4 bytes: the second line starts at 4.
        open_match("a.md", 2, 9..15);
        assert_eq!(names(&window), [file("notes.md"), file("a.md")]);
        assert_eq!(selection(rig), (13, 19));
        // A range past the end of the line stops at the line end.
        open_match("a.md", 2, 9..99);
        assert_eq!(selection(rig).0, 13);
        assert!(
            (19..=20).contains(&selection(rig).1),
            "{:?}",
            selection(rig)
        );
        // A range that starts after the line, or inside a character, puts the caret at its start.
        open_match("a.md", 2, 50..60);
        assert_eq!(selection(rig), (4, 4));
        open_match("d.md", 1, 1..2);
        assert_eq!(selection(rig), (0, 0));
        // Line 0 is the first line.
        open_match("a.md", 0, 0..1);
        assert_eq!(selection(rig), (0, 1));
        assert_eq!(names(&window).len(), 3);

        // Quick Open opens documents, hands other files to the shell and reports missing ones.
        rig.act(ShellAction::Overlay(OverlayAction::OpenFile(
            "b.md".to_owned(),
        )));
        assert_eq!(names(&window).len(), 4);
        rig.act(ShellAction::Overlay(OverlayAction::OpenFile(
            "pic.png".to_owned(),
        )));
        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenPath(root.join("pic.png"))]
        );
        rig.ok();
        rig.act(ShellAction::Overlay(OverlayAction::OpenFile(
            "gone.md".to_owned(),
        )));
        assert_eq!(rig.messages(), [text_of("app-error-not-found")]);
        assert_eq!(names(&window).len(), 4);

        // If every tab disappears while the error is shown, there is no tab to select in.
        let target = Rc::clone(&window);
        *rig.hook.borrow_mut() = Some(Box::new(move || {
            for key in keys(&target) {
                target.view.borrow_mut().close_tab(key);
            }
        }));
        rig.ok();
        open_match("gone.md", 1, 0..1);
        assert!(window.view.borrow().tabs().is_empty());
    });
}

#[test]
fn quick_open_lists_the_workspace_even_when_open_documents_lie_outside_it() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        // notes.md is open, but the workspace is the sub folder: it is not a recent file there.
        let sub = rig.work.join("sub");
        rig.controller.show_workspace(&window, &sub);
        assert!(handled(&rig.cmd(CommandId::SearchQuickOpen)));
        assert!(window.view.borrow().overlays().quick_open_visible());
    });
}

// ----- app commands ----------------------------------------------------------------------------

#[test]
fn the_folder_command_shows_the_chosen_folder_and_cancelling_changes_nothing() {
    with_rig(NO_AUTOSAVE, |rig| {
        let window = rig.window();
        rig.services.answer(DialogKind::PickFolder, Answer::Cancel);
        assert!(handled(&rig.cmd(CommandId::FileOpenFolder)));
        assert!(window.tree.borrow().is_none());

        let sub = rig.work.join("sub");
        rig.services
            .answer(DialogKind::PickFolder, Answer::Paths(vec![sub.clone()]));
        assert!(handled(&rig.cmd(CommandId::FileOpenFolder)));
        assert!(window.tree.borrow().is_some());
        assert_eq!(
            window.view.borrow().tabs()[0].workspace.as_deref(),
            Some(sub.as_path())
        );
    });
}

#[test]
fn exit_settings_and_the_logs_folder_ask_the_loop_owner_and_the_shell() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.requests();
        assert!(handled(&rig.cmd(CommandId::FileExit)));
        assert!(handled(&rig.cmd(CommandId::SettingsOpen)));
        let requests = rig.requests();
        assert!(requests.contains(&Request::Quit), "{requests:?}");
        assert!(requests.contains(&Request::OpenSettings), "{requests:?}");

        let logs = rig.controller.core().borrow().paths.logs_dir();
        assert!(handled(&rig.cmd(CommandId::SettingsOpenLogsFolder)));
        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenPath(logs.clone())]
        );
        assert!(logs.is_dir());
    });
}

#[test]
fn resetting_the_settings_needs_a_yes() {
    with_rig(NO_AUTOSAVE, |rig| {
        rig.controller.set_zoom(150);
        rig.answer(MessageBoxResult::No);
        assert!(handled(&rig.cmd(CommandId::SettingsReset)));
        assert_eq!(rig.zoom(), 150);
        rig.answer(MessageBoxResult::Yes);
        assert!(handled(&rig.cmd(CommandId::SettingsReset)));
        assert_eq!(rig.zoom(), 100);
    });
}

#[test]
fn saving_a_read_only_file_reports_the_failure() {
    with_rig(NO_AUTOSAVE, |rig| {
        let notes = rig.work.join("notes.md");
        set_readonly(&notes, true);
        rig.set_text("edited\n");
        rig.ok();
        let outcome = rig.cmd(CommandId::FileSave);
        set_readonly(&notes, false);
        match outcome {
            Dispatch::Failed(error) => assert_eq!(error.message_id(), "app-error-read-only"),
            other => panic!("{other:?}"),
        }
        assert_eq!(rig.messages(), [text_of("app-error-read-only")]);
        assert_eq!(std::fs::read_to_string(&notes).unwrap(), "# Notes\n");
    });
}
