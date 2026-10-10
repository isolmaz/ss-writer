//! Export, print and share through the real controller (spec §9): File → Export… opens the
//! export dialog over the window and its Export button runs the export with the chosen format;
//! printing lays the pages out and hands them to the printing seam (always on white paper);
//! sharing saves a document with unsaved changes first and hands the file to the sharing seam.
//! The seams and the dialogs are recording doubles; nothing is shown.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::replace_text;
use ssw_app::i18n::localizer_for;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::controller::Controller;
use ssw_app::ui::export::{PrintJob, Printing, Sharing};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_core::command::CommandId;
use ssw_render::scene::{Color, Paint, SceneItem};
use ssw_ui::access::Role;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard, MessageBoxResult, ServiceError};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::window::current_os_version;
use windows::Win32::Foundation::HWND;

const DOCUMENT: &str = "# Title\n\nSome body text.\n";

struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
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
            services,
            engine: MonoTextEngine::default(),
        };
        (self.body.take().expect("the body runs once"))(&rig);
        assert_eq!(
            rig.services.take_unexpected(),
            None,
            "a dialog nobody expected"
        );
        assert!(
            rig.services.unused_answers().is_empty(),
            "an answer nobody asked for"
        );
        0
    }
}

/// Runs `body` over a started application whose first window shows `notes.md` (holding
/// `DOCUMENT`); `settings` is the content of `settings.json`.
fn with_rig(settings: &str, body: impl FnOnce(&Rig, &Path)) {
    let tmp = tempfile::tempdir().unwrap();
    let work: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.md"), DOCUMENT).unwrap();
    let folder = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("settings.json"), settings).unwrap();
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
    let mut startup_dialogs = ScriptedServices::new().dialogs();
    let code = run(
        &args,
        process,
        &mut *startup_dialogs,
        None,
        &mut Host {
            body: Some(move |rig: &Rig| body(rig, &work)),
        },
    );
    assert_eq!(code, 0);
}

impl Rig {
    fn id(&self) -> ssw_app::workers::WindowId {
        self.controller.windows()[0].id
    }

    fn run(&self, command: CommandId) {
        self.controller.run_command(self.id(), command);
    }

    /// Delivers worker results until nothing is pending.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
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

    fn node_center(&self, role: Role, name: &str) -> Point {
        let window = Rc::clone(&self.controller.windows()[0]);
        let mut view = window.view.borrow_mut();
        view.paint(&mut RecordingCanvas::new(), &self.engine, false);
        let tree = view.access_tree(&self.engine, "SS Writer");
        tree.nodes()
            .iter()
            .find(|node| node.role == role && node.name == name)
            .unwrap_or_else(|| panic!("no {role:?} named {name:?}"))
            .bounds
            .center()
    }

    /// A click: what the window content reports is carried out like the loop owner does.
    fn click(&self, at: Point) {
        let window = Rc::clone(&self.controller.windows()[0]);
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            let event = PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, Some(PointerButton::Left), at)
            };
            let actions = window
                .view
                .borrow_mut()
                .event(&Event::Pointer(event), &self.engine);
            for action in actions {
                self.controller.handle_action(window.id, action);
            }
        }
    }

    fn answer_save(&self, target: &Path) {
        self.services.answer(
            DialogKind::SaveFile,
            Answer::Paths(vec![target.to_path_buf()]),
        );
    }

    fn answer_finished(&self) {
        self.services.answer(
            DialogKind::MessageBox,
            Answer::Button(MessageBoxResult::Cancel),
        );
    }
}

// ----- the export dialog -----------------------------------------------------------------------

#[test]
fn file_export_opens_the_dialog_and_its_export_button_exports_the_chosen_format() {
    with_rig("{}", |rig, work| {
        let window = Rc::clone(&rig.controller.windows()[0]);
        assert!(!window.view.borrow().overlays().export_visible());
        rig.run(CommandId::FileExport);
        assert!(window.view.borrow().overlays().export_visible());

        // The dialog starts from the Export settings (PDF by default); choose plain text.
        let txt = localizer_for("en").text("settings-opt-format-txt");
        let at = rig.node_center(Role::ListItem, &txt);
        rig.click(at);
        let target = work.join("exported.txt");
        rig.answer_save(&target);
        rig.answer_finished();
        let at = rig.node_center(Role::Button, "Export");
        rig.click(at);
        assert!(
            !window.view.borrow().overlays().export_visible(),
            "the dialog closes with Export"
        );
        rig.settle();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(
            text.contains("Title") && text.contains("Some body text."),
            "{text}"
        );
        let save = rig
            .services
            .dialog_log()
            .into_iter()
            .find(|dialog| dialog.kind == DialogKind::SaveFile)
            .expect("the target file was asked for");
        assert_eq!(
            save.message, "notes.txt",
            "the suggested name follows the format"
        );
    });
}

#[test]
fn cancel_closes_the_dialog_without_exporting() {
    with_rig("{}", |rig, work| {
        rig.run(CommandId::FileExport);
        let window = Rc::clone(&rig.controller.windows()[0]);
        let at = rig.node_center(Role::Button, "Cancel");
        rig.click(at);
        assert!(!window.view.borrow().overlays().export_visible());
        rig.settle();
        assert!(rig.services.dialog_log().is_empty());
        assert!(!work.join("exported.txt").exists());
    });
}

// ----- print and share -------------------------------------------------------------------------

struct RecordingPrinting(Rc<RefCell<Vec<PrintJob>>>);

impl Printing for RecordingPrinting {
    fn print(&mut self, job: PrintJob) -> Result<Option<usize>, ServiceError> {
        let pages = job.pages.len();
        self.0.borrow_mut().push(job);
        Ok(Some(pages))
    }
}

/// What the sharing seam saw: the file, the title and the file's text at that moment.
type Shared = (PathBuf, String, String);

struct RecordingSharing(Rc<RefCell<Vec<Shared>>>);

impl Sharing for RecordingSharing {
    fn share(
        &mut self,
        _owner: Option<HWND>,
        file: &Path,
        title: &str,
    ) -> Result<(), ServiceError> {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        self.0
            .borrow_mut()
            .push((file.to_path_buf(), title.to_owned(), text));
        Ok(())
    }
}

#[test]
fn printing_lays_the_pages_out_and_hands_them_to_the_printing_seam_on_white_paper() {
    // The dark export theme is for files: paper is white.
    with_rig(r#"{"export":{"light_theme":false}}"#, |rig, _| {
        let jobs = Rc::new(RefCell::new(Vec::new()));
        rig.controller
            .set_printing(Box::new(RecordingPrinting(Rc::clone(&jobs))));
        rig.run(CommandId::FilePrint);
        rig.settle();
        let jobs = jobs.borrow();
        assert_eq!(jobs.len(), 1, "one print job");
        let job = &jobs[0];
        assert_eq!(job.name, "notes");
        assert_eq!(job.pages.len(), 1);
        let page = &job.pages[0];
        assert!(
            (page.size.width - 595.28).abs() < 0.1,
            "A4: {:?}",
            page.size
        );
        match &page.items[0] {
            SceneItem::Fill(paper) => assert_eq!(paper.paint, Paint::Solid(Color::WHITE)),
            other => panic!("paper expected first, got {other:?}"),
        }
        assert!(
            page.items
                .iter()
                .any(|item| matches!(item, SceneItem::Glyphs(_) | SceneItem::Group(_))),
            "the page has content"
        );
    });
}

#[test]
fn sharing_saves_unsaved_changes_first_and_shares_the_file() {
    with_rig("{}", |rig, work| {
        let shared = Rc::new(RefCell::new(Vec::new()));
        rig.controller
            .set_sharing(Box::new(RecordingSharing(Rc::clone(&shared))));
        let entry = {
            let window = Rc::clone(&rig.controller.windows()[0]);
            let view = window.view.borrow();
            Rc::clone(&view.active_tab().expect("a tab").entry)
        };
        assert!(replace_text(
            &mut entry.document().borrow_mut(),
            "# Edited\n"
        ));
        assert!(entry.is_dirty());
        rig.run(CommandId::FileShare);
        let shared = shared.borrow();
        assert_eq!(shared.len(), 1);
        let (file, title, text) = &shared[0];
        assert_eq!(file, &entry.path().unwrap());
        assert_eq!(file.file_name(), work.join("notes.md").file_name());
        assert!(title.contains("notes"), "{title}");
        assert_eq!(
            text, "# Edited\n",
            "the file was saved before it was shared"
        );
        assert!(!entry.is_dirty());
    });
}
