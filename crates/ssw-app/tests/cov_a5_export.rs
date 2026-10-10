//! Export, print and share through the real controller, and the adapters behind its seams
//! (spec §9): what the user is told when there is no document, no printer, no sheet, a
//! picture that cannot be laid out, a target that cannot be written, a window that no longer
//! takes work; how the taskbar progress follows several exports; the Mermaid menu; the export
//! dialog's input routing. Scripted dialogs and shell, a headless controller, hidden windows
//! that are never shown; nothing reaches the screen, the printer queue or the share sheet.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::documents::{DocEntry, replace_text};
use ssw_app::i18n::localizer_for;
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::context_menus::Action;
use ssw_app::ui::controller::{Controller, Request};
use ssw_app::ui::export::{
    DiagramFormat, ExportHost, ExportState, PrintJob, Printing, Sharing, SystemPrinting,
    SystemSharing,
};
use ssw_app::ui::overlays::OverlayEnv;
use ssw_app::ui::scripted_services::{
    Answer, DialogKind, DialogRecord, ScriptedServices, ShellRecord,
};
use ssw_app::ui::shell_view::ShellAction;
use ssw_core::command::CommandId;
use ssw_core::settings::ExportSettings;
use ssw_core::text::TextRange;
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::blocks_media::MediaEvent;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, FocusEvent, PointerButton, PointerEvent, PointerEventKind};
use ssw_ui::services::{
    Clock, FakeClock, MemoryClipboard, MessageBoxResult, ServiceError, TimerId,
};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{FocusManager, PointerCapture, Requests};
use ssw_ui::widgets::SharedClipboard;
use ssw_ui::widgets::anim::Motion;
use ssw_win::com::ComGuard;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{Driver, GfxDevices};
use ssw_win::taskbar::Progress;
use ssw_win::wic::Wic;
use ssw_win::window::current_os_version;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

const DOCUMENT: &str = "# Title\n\nSome body text.\n";

// ----- the controller without native windows ---------------------------------------------------

struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
}

struct Host<B> {
    body: Option<B>,
}

impl<B: FnOnce(Rig)> ShellHost for Host<B> {
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
            services: services.clone(),
        };
        (self.body.take().expect("the body runs once"))(rig);
        assert_eq!(services.take_unexpected(), None, "a dialog nobody expected");
        assert!(
            services.unused_answers().is_empty(),
            "an answer nobody asked for"
        );
        0
    }
}

/// Runs `body` over a started application whose first window shows `notes.md` (holding
/// `DOCUMENT`), with the folder the document lives in.
fn with_rig_owned(body: impl FnOnce(Rig, PathBuf)) {
    let tmp = tempfile::tempdir().unwrap();
    let work: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("notes.md"), DOCUMENT).unwrap();
    let folder = tmp.path().join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("settings.json"), "{}").unwrap();
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
            body: Some(move |rig: Rig| body(rig, work)),
        },
    );
    assert_eq!(code, 0);
}

fn with_rig(body: impl FnOnce(&Rig, &Path)) {
    with_rig_owned(|rig, work| body(&rig, &work));
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

    fn entry(&self) -> Rc<DocEntry> {
        let window = Rc::clone(&self.controller.windows()[0]);
        let view = window.view.borrow();
        Rc::clone(&view.active_tab().expect("a tab").entry)
    }

    fn set_text(&self, text: &str) {
        replace_text(&mut self.entry().document().borrow_mut(), text);
    }

    fn answer_save(&self, target: &Path) {
        self.services.answer(
            DialogKind::SaveFile,
            Answer::Paths(vec![target.to_path_buf()]),
        );
    }

    fn answer_box(&self, button: MessageBoxResult) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(button));
    }

    /// The message boxes shown so far.
    fn boxes(&self) -> Vec<DialogRecord> {
        self.services
            .dialog_log()
            .into_iter()
            .filter(|dialog| dialog.kind == DialogKind::MessageBox)
            .collect()
    }

    /// The taskbar progress the controller asked for since the last call.
    fn progress(&self) -> Vec<Progress> {
        let mut seen = Vec::new();
        while let Some(request) = self.controller.take_request() {
            if let Request::TaskbarProgress(_, progress) = request {
                seen.push(progress);
            }
        }
        seen
    }
}

/// Prints into a list, or fails like a printer that is on fire.
struct ScriptedPrinting {
    jobs: Rc<RefCell<Vec<PrintJob>>>,
    fail: bool,
}

impl Printing for ScriptedPrinting {
    fn print(&mut self, job: PrintJob) -> Result<Option<usize>, ServiceError> {
        self.jobs.borrow_mut().push(job);
        if self.fail {
            Err(ServiceError::Failed {
                service: "print",
                message: "the printer is on fire".to_owned(),
            })
        } else {
            Ok(None)
        }
    }
}

struct RefusingSheet;

impl Sharing for RefusingSheet {
    fn share(
        &mut self,
        _owner: Option<HWND>,
        _file: &Path,
        _title: &str,
    ) -> Result<(), ServiceError> {
        Err(ServiceError::Failed {
            service: "share",
            message: "the sheet refused".to_owned(),
        })
    }
}

// ----- the export commands -----------------------------------------------------------------------

#[test]
fn every_export_command_asks_for_a_file_named_after_the_document_and_its_format() {
    with_rig(|rig, work| {
        let commands = [
            (CommandId::ExportPdf, "pdf"),
            (CommandId::ExportPng, "png"),
            (CommandId::ExportJpg, "jpg"),
            (CommandId::ExportHtml, "html"),
            (CommandId::ExportEpub, "epub"),
            (CommandId::ExportDocx, "docx"),
            (CommandId::ExportOdt, "odt"),
            (CommandId::ExportRtf, "rtf"),
            (CommandId::ExportTxt, "txt"),
            (CommandId::ExportMarkdownWithAttachments, "md"),
            // A TextBundle is offered as a package first.
            (CommandId::ExportTextBundle, "textpack"),
        ];
        for (command, _) in commands {
            rig.services.answer(DialogKind::SaveFile, Answer::Cancel);
            rig.run(command);
        }
        let asked: Vec<(DialogKind, String)> = rig
            .services
            .dialog_log()
            .into_iter()
            .map(|dialog| (dialog.kind, dialog.message))
            .collect();
        let expected: Vec<(DialogKind, String)> = commands
            .iter()
            .map(|(_, extension)| (DialogKind::SaveFile, format!("notes.{extension}")))
            .collect();
        assert_eq!(asked, expected);
        // Cancelling the dialog exports nothing.
        assert_eq!(std::fs::read_dir(work).unwrap().count(), 1);
        rig.settle();
        assert!(rig.progress().is_empty());
    });
}

#[test]
fn exporting_over_the_document_itself_is_refused_in_any_spelling() {
    with_rig(|rig, work| {
        for name in ["notes.md", "NOTES.MD"] {
            rig.answer_save(&work.join(name));
            rig.answer_box(MessageBoxResult::Ok);
            rig.run(CommandId::ExportMarkdownWithAttachments);
        }
        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 2);
        for shown in &boxes {
            assert_eq!(shown.title, "Export failed");
            assert!(
                shown.message.contains("could not be exported as"),
                "{}",
                shown.message
            );
            assert!(shown.message.contains("Markdown with attachments"));
            assert!(shown.message.contains("Choose a different file"));
        }
        assert_eq!(
            std::fs::read_to_string(work.join("notes.md")).unwrap(),
            DOCUMENT
        );
        rig.settle();
    });
}

#[test]
fn an_export_that_cannot_write_names_the_file() {
    with_rig(|rig, work| {
        let target = work.join("missing").join("out.txt");
        rig.answer_save(&target);
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::ExportTxt);
        rig.settle();
        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].title, "Export failed");
        assert!(boxes[0].message.contains("could not be written"));
        assert!(boxes[0].message.contains(&*target.to_string_lossy()));
        assert!(boxes[0].message.contains("Plain text"));
        assert!(!work.join("missing").exists());
    });
}

#[test]
fn a_picture_that_cannot_be_laid_out_is_reported_for_export_and_print() {
    with_rig(|rig, work| {
        std::fs::write(work.join("broken.png"), b"this is not a picture").unwrap();
        rig.set_text("![x](broken.png)\n");

        rig.answer_save(&work.join("out.pdf"));
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::ExportPdf);
        rig.settle();
        assert!(!work.join("out.pdf").exists());

        let jobs = Rc::new(RefCell::new(Vec::new()));
        rig.controller.set_printing(Box::new(ScriptedPrinting {
            jobs: Rc::clone(&jobs),
            fail: false,
        }));
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::FilePrint);
        rig.settle();
        assert!(jobs.borrow().is_empty(), "nothing reaches the printer");

        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].title, "Export failed");
        assert!(boxes[0].message.contains("layout failed"), "{boxes:?}");
        assert!(boxes[0].message.contains("PDF"));
        assert_eq!(boxes[1].title, "Print");
        assert!(boxes[1].message.contains("could not be printed"));
        assert!(boxes[1].message.contains("layout failed"), "{boxes:?}");
    });
}

#[test]
fn a_closed_window_refuses_the_work_and_the_taskbar_progress_ends() {
    with_rig(|rig, work| {
        rig.controller
            .core()
            .borrow()
            .workers
            .cancel_window(rig.id());
        rig.answer_save(&work.join("out.txt"));
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::ExportTxt);
        assert_eq!(rig.progress(), [Progress::Indeterminate, Progress::None]);
        assert!(!work.join("out.txt").exists());

        let jobs = Rc::new(RefCell::new(Vec::new()));
        rig.controller.set_printing(Box::new(ScriptedPrinting {
            jobs: Rc::clone(&jobs),
            fail: false,
        }));
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::FilePrint);
        assert_eq!(rig.progress(), [Progress::Indeterminate, Progress::None]);

        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 2);
        assert!(
            boxes[0]
                .message
                .contains("The export could not be started.")
        );
        assert!(
            boxes[0]
                .message
                .contains("the window of the job was closed")
        );
        assert_eq!(boxes[1].title, "Print");
        assert!(boxes[1].message.contains("could not be printed"));
        assert!(
            boxes[1]
                .message
                .contains("the window of the job was closed")
        );
        assert!(jobs.borrow().is_empty());
    });
}

#[test]
fn the_taskbar_progress_ends_with_the_last_of_several_exports() {
    with_rig(|rig, work| {
        let (first, second) = (work.join("a.txt"), work.join("b.txt"));
        rig.answer_save(&first);
        rig.answer_save(&second);
        rig.run(CommandId::ExportTxt);
        rig.run(CommandId::ExportTxt);
        rig.answer_box(MessageBoxResult::Cancel);
        rig.answer_box(MessageBoxResult::Cancel);
        rig.settle();
        assert_eq!(
            rig.progress(),
            [
                Progress::Indeterminate,
                Progress::Indeterminate,
                Progress::None
            ]
        );
        assert!(std::fs::read_to_string(first).unwrap().contains("Title"));
        assert!(std::fs::read_to_string(second).unwrap().contains("Title"));
        assert_eq!(rig.boxes().len(), 2);
    });
}

#[test]
fn a_long_document_reports_its_pages_and_yes_opens_the_first() {
    with_rig(|rig, work| {
        rig.set_text(&"A line of text.\n\n".repeat(200));
        rig.answer_save(&work.join("long.png"));
        rig.answer_box(MessageBoxResult::Yes);
        rig.run(CommandId::ExportPng);
        rig.settle();
        let pages = std::fs::read_dir(work)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("long-"))
            .count();
        assert!(pages > 1, "{pages}");
        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].title, "Export finished");
        assert!(
            boxes[0].message.contains("pages of"),
            "{}",
            boxes[0].message
        );
        assert!(boxes[0].message.contains(&pages.to_string()));
        assert!(boxes[0].message.contains(&*work.to_string_lossy()));
        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenPath(work.join("long-1.png"))]
        );
    });
}

#[test]
fn the_result_of_an_export_is_dropped_with_its_controller() {
    with_rig_owned(|rig, work| {
        let target = work.join("late.txt");
        rig.answer_save(&target);
        rig.run(CommandId::ExportTxt);
        let workers = Rc::clone(&rig.controller.core().borrow().workers);
        let Rig {
            controller,
            services,
        } = rig;
        let Ok(controller) = Rc::try_unwrap(controller) else {
            panic!("the controller is shared");
        };
        drop(controller);
        let deadline = Instant::now() + Duration::from_secs(60);
        while workers.pending() > 0 {
            workers.poll();
            assert!(Instant::now() < deadline, "the worker did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The worker ran and its result was delivered to nobody: no message box.
        assert_eq!(workers.stats().delivered, 1);
        assert!(std::fs::read_to_string(target).unwrap().contains("Title"));
        assert!(
            services
                .dialog_log()
                .iter()
                .all(|dialog| dialog.kind == DialogKind::SaveFile)
        );
    });
}

// ----- the Mermaid menu -------------------------------------------------------------------------

fn mermaid_action(rig: &Rig, source: &str) -> ShellAction {
    let key = {
        let window = Rc::clone(&rig.controller.windows()[0]);
        let view = window.view.borrow();
        view.active_tab().expect("a tab").key
    };
    ShellAction::Editor(
        key,
        EditorEvent::Media(MediaEvent::MermaidMenu {
            range: TextRange::new(0, 0),
            source: source.to_owned(),
            anchor: Rect::new(10.0, 10.0, 0.0, 0.0),
        }),
    )
}

#[test]
fn a_diagram_exports_as_png_or_svg_from_its_menu() {
    with_rig(|rig, work| {
        let source = "graph TD\n  A --> B\n";
        let popup = rig
            .controller
            .popup_for(rig.id(), &mermaid_action(rig, source))
            .unwrap();
        let ids = popup.ids();
        assert_eq!(ids.len(), 2);
        for (id, format) in [(ids[0], DiagramFormat::Png), (ids[1], DiagramFormat::Svg)] {
            assert_eq!(
                popup.action(id),
                Some(&Action::MermaidExport {
                    source: source.to_owned(),
                    format,
                })
            );
        }

        let png = work.join("d.png");
        rig.answer_save(&png);
        rig.answer_box(MessageBoxResult::Yes);
        rig.controller.run_popup_choice(rig.id(), &popup, ids[0]);
        rig.settle();
        assert!(
            std::fs::read(&png)
                .unwrap()
                .starts_with(b"\x89PNG\r\n\x1a\n")
        );

        let svg = work.join("d.svg");
        rig.answer_save(&svg);
        rig.answer_box(MessageBoxResult::No);
        rig.controller.run_popup_choice(rig.id(), &popup, ids[1]);
        rig.settle();
        assert!(std::fs::read_to_string(&svg).unwrap().contains("<svg"));

        let asked: Vec<String> = rig
            .services
            .dialog_log()
            .into_iter()
            .filter(|dialog| dialog.kind == DialogKind::SaveFile)
            .map(|dialog| dialog.message)
            .collect();
        assert_eq!(asked, ["Diagram.png", "Diagram.svg"]);
        assert_eq!(
            rig.services.shell_log(),
            [ShellRecord::OpenPath(png), ShellRecord::Reveal(svg)]
        );
    });
}

#[test]
fn a_diagram_that_cannot_be_drawn_or_whose_dialog_is_cancelled_exports_nothing() {
    with_rig(|rig, work| {
        let popup = rig
            .controller
            .popup_for(rig.id(), &mermaid_action(rig, "sankey-beta\n  a,b,1\n"))
            .unwrap();
        let png = popup.ids()[0];

        rig.services.answer(DialogKind::SaveFile, Answer::Cancel);
        rig.controller.run_popup_choice(rig.id(), &popup, png);
        rig.settle();
        assert!(rig.progress().is_empty(), "no job was started");

        let target = work.join("none.png");
        rig.answer_save(&target);
        rig.answer_box(MessageBoxResult::Ok);
        rig.controller.run_popup_choice(rig.id(), &popup, png);
        rig.settle();
        assert!(!target.exists());
        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].title, "Export failed");
        assert!(boxes[0].message.contains("The diagram could not be drawn"));
        assert!(boxes[0].message.contains("PNG image"));
    });
}

// ----- no document ------------------------------------------------------------------------------

#[test]
fn a_window_without_a_document_has_nothing_to_export_print_or_share() {
    with_rig(|rig, _| {
        let cell = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0);
        let commands = [
            CommandId::FileExport,
            CommandId::ExportPdf,
            CommandId::FilePrint,
            CommandId::FileShare,
        ];
        for command in commands {
            rig.answer_box(MessageBoxResult::Ok);
            rig.controller.run_command(cell.id, command);
        }
        let action = ShellAction::Editor(
            0,
            EditorEvent::Media(MediaEvent::MermaidMenu {
                range: TextRange::new(0, 0),
                source: "graph TD\n  A --> B\n".to_owned(),
                anchor: Rect::new(0.0, 0.0, 0.0, 0.0),
            }),
        );
        let popup = rig.controller.popup_for(cell.id, &action).unwrap();
        rig.answer_box(MessageBoxResult::Ok);
        rig.controller
            .run_popup_choice(cell.id, &popup, popup.ids()[0]);

        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 5);
        for shown in &boxes {
            assert_eq!(shown.title, "Nothing to export");
            assert_eq!(shown.message, "Open a document first.");
        }
        assert!(!cell.view.borrow().overlays().export_visible());
        assert!(rig.progress().is_empty());
    });
}

// ----- print and share ------------------------------------------------------------------------

#[test]
fn printing_tells_the_user_when_there_is_no_printer_or_the_printer_fails() {
    with_rig(|rig, _| {
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::FilePrint);
        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].title, "Print");
        assert_eq!(
            boxes[0].message,
            "Printing is not available in this window."
        );

        let jobs = Rc::new(RefCell::new(Vec::new()));
        rig.controller.set_printing(Box::new(ScriptedPrinting {
            jobs: Rc::clone(&jobs),
            fail: true,
        }));
        // The seam is handed back after a failure: the second print reaches it again.
        for _ in 0..2 {
            rig.answer_box(MessageBoxResult::Ok);
            rig.run(CommandId::FilePrint);
            rig.settle();
        }
        assert_eq!(jobs.borrow().len(), 2);
        assert_eq!(jobs.borrow()[0].name, "notes");
        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 3);
        for shown in &boxes[1..] {
            assert_eq!(shown.title, "Print");
            assert!(shown.message.contains("could not be printed"));
            assert!(shown.message.contains("the printer is on fire"));
        }
        // A user who cancelled the print dialog hears nothing.
        let cancelled = Rc::new(RefCell::new(Vec::new()));
        rig.controller.set_printing(Box::new(ScriptedPrinting {
            jobs: Rc::clone(&cancelled),
            fail: false,
        }));
        rig.run(CommandId::FilePrint);
        rig.settle();
        assert_eq!(cancelled.borrow().len(), 1);
        assert_eq!(rig.boxes().len(), 3);
    });
}

#[test]
fn sharing_tells_the_user_when_it_cannot_share() {
    with_rig(|rig, _| {
        // No sheet in this window.
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::FileShare);
        // A sheet that refuses.
        rig.controller.set_sharing(Box::new(RefusingSheet));
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::FileShare);
        // A document without a file: only files can be shared.
        rig.run(CommandId::FileNew);
        rig.answer_box(MessageBoxResult::Ok);
        rig.run(CommandId::FileShare);

        let boxes = rig.boxes();
        assert_eq!(boxes.len(), 3);
        assert_eq!(boxes[0].title, "Share");
        assert_eq!(boxes[0].message, "Sharing is not available in this window.");
        assert_eq!(boxes[1].title, "Share");
        assert!(boxes[1].message.contains("could not be shared"));
        assert!(boxes[1].message.contains("the sheet refused"));
        assert_eq!(boxes[2].title, "Share");
        assert!(boxes[2].message.contains("Save the document first"));
    });
}

// ----- the adapters behind the seams ------------------------------------------------------------

/// A top-level window that is never shown: the sharer needs a window handle.
fn hidden_window() -> HWND {
    // SAFETY: creates a window of the system `STATIC` class without `WS_VISIBLE`.
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!("ssw-app share test"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        )
    }
    .unwrap()
}

fn failed_share(result: Result<(), ServiceError>) -> String {
    match result {
        Err(ServiceError::Failed { service, message }) => {
            assert_eq!(service, "share");
            message
        }
        other => panic!("expected a failed share, got {other:?}"),
    }
}

#[test]
fn the_system_sharing_needs_a_window_and_a_file_before_it_opens_any_sheet() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let mut sharing = SystemSharing::default();
    let missing = std::env::temp_dir().join("ssw-app-share-never-existed.md");
    assert!(matches!(
        sharing.share(None, &missing, "Never"),
        Err(ServiceError::Unavailable { service: "share" })
    ));

    // A window and a file that does not exist: the file is resolved before the sheet opens.
    // The second request of the window reuses its sharer.
    let window = hidden_window();
    for _ in 0..2 {
        let message = failed_share(sharing.share(Some(window), &missing, "Never"));
        assert!(!message.is_empty());
    }
    drop(sharing);
    // SAFETY: destroys the window created above on its own thread.
    let _ = unsafe { DestroyWindow(window) };
}

#[test]
fn the_system_printing_refuses_a_job_without_pages_before_any_dialog() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let devices = GfxDevices::create_on(Driver::Warp, 1).unwrap();
    let wic = Wic::new().unwrap();
    let text = DWriteEngine::new().unwrap();
    let mut printing = SystemPrinting::new(devices, wic, text);
    assert_eq!(format!("{printing:?}"), "SystemPrinting { .. }");
    let result = printing.print(PrintJob {
        owner: None,
        name: "empty".to_owned(),
        pages: Vec::new(),
    });
    match result {
        Err(ServiceError::Failed { service, message }) => {
            assert_eq!(service, "print");
            assert!(message.contains("no pages to print"), "{message}");
        }
        other => panic!("expected a failed print, got {other:?}"),
    }
}

#[test]
fn the_export_state_shows_no_export_running_at_first() {
    assert_eq!(
        format!("{:?}", ExportState::default()),
        "ExportState { running: {}, .. }"
    );
}

// ----- the export dialog host -------------------------------------------------------------------

struct HostEnv {
    theme: Theme,
    text: MonoTextEngine,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
}

impl HostEnv {
    fn new() -> Self {
        Self {
            theme: Theme::light(DEFAULT_ACCENT),
            text: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
        }
    }

    /// Whether the host took `event`.
    fn handled(&mut self, host: &mut ExportHost, event: &Event) -> bool {
        let mut env = OverlayEnv {
            theme: &self.theme,
            scale: 1.0,
            text: &self.text,
            focus: &mut self.focus,
            capture: &mut self.capture,
            requests: &mut self.requests,
        };
        host.event(event, &mut env).handled
    }
}

fn new_host() -> ExportHost {
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    ExportHost::new(Rc::new(localizer_for("en")), clipboard)
}

fn click() -> Event {
    Event::Pointer(PointerEvent::mouse(
        PointerEventKind::Down,
        Some(PointerButton::Left),
        Point::new(1.0, 1.0),
    ))
}

#[test]
fn a_closed_export_host_leaves_every_event_to_what_is_behind_it() {
    let mut host = new_host();
    let mut env = HostEnv::new();
    assert!(!host.is_open());
    assert!(host.dialog().is_none());
    assert_eq!(
        format!("{host:?}"),
        "ExportHost { open: false, request: None, .. }"
    );
    assert!(!env.handled(&mut host, &click()));
    assert!(!env.handled(&mut host, &Event::Timer(TimerId::new(1))));
}

#[test]
fn an_open_export_host_is_modal_but_lets_timers_frames_and_focus_pass() {
    let mut host = new_host();
    let mut env = HostEnv::new();
    host.open(&ExportSettings::default(), Motion::default());
    assert!(host.is_open());
    assert!(host.dialog().is_some());
    assert_eq!(
        format!("{host:?}"),
        "ExportHost { open: true, request: None, .. }"
    );
    assert!(
        env.handled(&mut host, &click()),
        "nothing behind it is clicked"
    );
    assert!(!env.handled(&mut host, &Event::Timer(TimerId::new(1))));
    assert!(!env.handled(&mut host, &Event::AnimationFrame(Duration::from_millis(16))));
    assert!(!env.handled(&mut host, &Event::Focus(FocusEvent::Gained)));
    assert!(host.is_open());
}
