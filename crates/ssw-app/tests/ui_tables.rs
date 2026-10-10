//! The table commands that need the host (spec §2.8, §9): "Copy as Markdown / CSV / HTML" put
//! the table around the caret on the clipboard and "Edit as raw Markdown" is accepted, all
//! through the `TableControl` of the active editor, which the editor setup hands to the tab.
//! Runs the real controller over a started application in a temporary profile.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::commands::Dispatch;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::Controller;
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::shell_view::EditorSetup;
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_ui::editor::EditorView;
use ssw_ui::editor::blocks_table::{self, TableStrings};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;

const TABLE: &str =
    "before\n\n| Name | Qty |\n|:-----|----:|\n| Apple | 3 |\n| Pear | 12 |\n\nafter\n";

struct Host<B> {
    body: Option<B>,
    clipboard: SharedClipboard,
}

impl<B: FnOnce(&Rc<Controller>, &SharedClipboard)> ShellHost for Host<B> {
    fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
        let services = ScriptedServices::new();
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller = Controller::new(
            core,
            services.dialogs(),
            services.shell(),
            Rc::clone(&self.clipboard),
            clock,
        );
        controller.open_plan(&plan);
        (self.body.take().expect("the body runs once"))(&controller, &self.clipboard);
        assert_eq!(services.take_unexpected(), None, "a dialog nobody expected");
        0
    }
}

/// Runs `body` over a started application whose first window shows a file holding `text`.
fn with_controller(text: &str, body: impl FnOnce(&Rc<Controller>, &SharedClipboard)) {
    let tmp = tempfile::tempdir().unwrap();
    let work: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.md"), text).unwrap();
    let mut args = Args {
        paths: vec![work.join("notes.md")],
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
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    let mut startup_dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process,
        &mut *startup_dialogs,
        None,
        &mut Host {
            body: Some(body),
            clipboard,
        },
    );
    assert_eq!(code, 0);
}

/// What the platform layer does for every editor: registers the table renderer and hands its
/// control to the tab.
fn table_setup() -> EditorSetup {
    Rc::new(|editor: &mut EditorView| {
        Some(blocks_table::register_with(
            editor.registry_mut(),
            TableStrings::default(),
        ))
    })
}

fn put_caret(controller: &Controller, at: usize) {
    let window = Rc::clone(&controller.windows()[0]);
    let mut view = window.view.borrow_mut();
    view.active_editor_mut()
        .expect("an editor")
        .go_to(Selection::caret(at));
}

fn run_table(controller: &Controller, command: CommandId) -> bool {
    let id = controller.windows()[0].id;
    matches!(controller.run_command(id, command), Dispatch::Handled)
}

#[test]
fn copy_as_csv_markdown_and_html_put_the_table_around_the_caret_on_the_clipboard() {
    with_controller(TABLE, |controller, clipboard| {
        let window = Rc::clone(&controller.windows()[0]);
        window.view.borrow_mut().set_editor_setup(table_setup());
        assert!(
            window.view.borrow().tabs()[0].table.is_some(),
            "the setup gave the open tab its table control"
        );

        // Outside the table nothing is copied and the command is not handled.
        put_caret(controller, 0);
        assert!(!run_table(controller, CommandId::TableCopyAsCsv));
        assert_eq!(clipboard.borrow().text().unwrap(), None);

        put_caret(controller, TABLE.find("Pear").unwrap());
        assert!(run_table(controller, CommandId::TableCopyAsCsv));
        assert_eq!(
            clipboard.borrow().text().unwrap(),
            Some("Name,Qty\r\nApple,3\r\nPear,12".to_owned())
        );

        assert!(run_table(controller, CommandId::TableCopyAsMarkdown));
        let markdown = clipboard.borrow().text().unwrap().expect("text");
        assert_eq!(markdown.lines().count(), 4, "{markdown}");
        assert!(markdown.starts_with("| Name"), "{markdown}");
        assert!(
            markdown.contains("Pear") && markdown.contains("12"),
            "{markdown}"
        );

        assert!(run_table(controller, CommandId::TableCopyAsHtml));
        let html = clipboard.borrow().html().unwrap().expect("html");
        assert!(html.starts_with("<table>"), "{html}");
        assert!(
            html.contains("Name</th>") && html.contains("Pear"),
            "{html}"
        );
        assert_eq!(clipboard.borrow().text().unwrap(), Some(html));
    });
}

#[test]
fn edit_as_raw_markdown_is_handled_in_a_table_only() {
    with_controller(TABLE, |controller, _clipboard| {
        let window = Rc::clone(&controller.windows()[0]);
        window.view.borrow_mut().set_editor_setup(table_setup());
        put_caret(controller, 0);
        assert!(!run_table(controller, CommandId::TableEditRaw));
        put_caret(controller, TABLE.find("Apple").unwrap());
        assert!(run_table(controller, CommandId::TableEditRaw));
    });
}

#[test]
fn an_editor_without_a_table_renderer_leaves_the_commands_unhandled() {
    with_controller(TABLE, |controller, clipboard| {
        // The headless application has no platform editor setup: there is no table control.
        let window = Rc::clone(&controller.windows()[0]);
        assert!(window.view.borrow().tabs()[0].table.is_none());
        put_caret(controller, TABLE.find("Pear").unwrap());
        for command in [
            CommandId::TableCopyAsCsv,
            CommandId::TableCopyAsMarkdown,
            CommandId::TableCopyAsHtml,
            CommandId::TableEditRaw,
        ] {
            assert!(!run_table(controller, command), "{command}");
        }
        assert_eq!(clipboard.borrow().text().unwrap(), None);
    });
}

#[test]
fn a_tab_opened_after_the_setup_gets_its_own_table_control() {
    with_controller(TABLE, |controller, clipboard| {
        let window = Rc::clone(&controller.windows()[0]);
        window.view.borrow_mut().set_editor_setup(table_setup());
        let entry = controller.docs().new_untitled();
        entry
            .document()
            .borrow_mut()
            .apply(ssw_core::edit::Transaction::single(
                ssw_core::edit::Edit::insert(0, "| A | B |\n|---|---|\n| 1 | 2 |\n"),
                Selection::caret(0),
                Selection::caret(0),
            ))
            .unwrap();
        window.view.borrow_mut().add_tab(entry, None);
        assert!(window.view.borrow().active_tab().unwrap().table.is_some());
        put_caret(controller, 2);
        assert!(run_table(controller, CommandId::TableCopyAsCsv));
        assert_eq!(
            clipboard.borrow().text().unwrap(),
            Some("A,B\r\n1,2".to_owned())
        );
    });
}
