//! Copy, cut and paste of the editor (spec §2.3, §9) through the real controller over a started
//! application in a temporary profile: `Ctrl+C` / `Ctrl+X` / `Ctrl+V` / `Ctrl+Shift+V` reach the
//! controller through the window content, copies put text, HTML, RTF and SS Writer's own
//! Markdown on a clipboard held in memory, pastes choose between that Markdown, pictures, files,
//! code, HTML and text, and pictures and files end up in the attachment folder of the document.
//! Nothing touches the system clipboard: the clipboard is a [`MemoryClipboard`] behind a wrapper
//! that can refuse single operations. The pure parts (key table, attachment folders, relative
//! destinations, numbered names, local images) are tested over a temporary folder, with
//! `FaultFs` where a file call has to fail.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};
use ssw_app::app::{AppCore, ShellHost, run};
use ssw_app::args::Args;
use ssw_app::commands::Dispatch;
use ssw_app::documents::{DocEntry, DocEnv, Documents, replace_text};
use ssw_app::startup::ProcessEnv;
use ssw_app::ui::clipboard::{
    AttachmentTarget, CopyKind, FORMAT_MARKDOWN, LocalImages, Paste, copy_kind, copy_to_folder,
    key_command, plan_paste, relative_destination, save_numbered,
};
use ssw_app::ui::controller::{Controller, WindowCell};
use ssw_app::ui::scripted_services::{Answer, DialogKind, ScriptedServices};
use ssw_app::workers::WindowId;
use ssw_convert::assets::{AssetProvider, data_uri};
use ssw_convert::import::Asset;
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::keymap::vk;
use ssw_core::settings::{AttachmentLocation, Attachments};
use ssw_io::fs::{FaultFs, FsError, FsFailure, FsOp, RealFs, win32};
use ssw_ui::geometry::Size;
use ssw_ui::input::{Event, KeyEvent, Modifiers};
use ssw_ui::services::{
    Clipboard, ClipboardContent, ClipboardFormat, Clock, FakeClock, MemoryClipboard,
    MessageBoxResult, ServiceError,
};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::widgets::SharedClipboard;
use ssw_win::gfx::Driver;
use ssw_win::services::clipboard::FORMAT_VSCODE_EDITOR_DATA;
use ssw_win::window::current_os_version;

const NOTES: &str = "# Notes\n\nSome **bold** text and `code`.\n\nEnd.\n";
const INTRO: &str = "Intro: \nEnd\n";
const NO_AUTOSAVE: &str = r#"{"general":{"autosave":false}}"#;
/// What the user is told when a file call of a paste fails (`app-error-file`).
const FILE_ERROR: &str = "The file could not be read or written.";

// ----- the clipboard ---------------------------------------------------------------------------

/// A [`MemoryClipboard`] that remembers what was set last and refuses the operations it was told
/// to (`"set"`, `"formats"`, `"text"`, `"html"`, `"image"`, `"files"`, `"custom:<name>"`), like a
/// system clipboard another process holds open.
#[derive(Debug, Default)]
struct Board {
    memory: MemoryClipboard,
    last: ClipboardContent,
    failing: HashSet<String>,
}

impl Board {
    fn with(content: ClipboardContent) -> Self {
        let mut board = Self::default();
        board.set(content).expect("a fresh board accepts content");
        board
    }

    fn fail(&mut self, operation: &str) {
        self.failing.insert(operation.to_owned());
    }

    fn check(&self, operation: &str) -> Result<(), ServiceError> {
        if self.failing.contains(operation) {
            Err(ServiceError::Unavailable {
                service: "clipboard",
            })
        } else {
            Ok(())
        }
    }
}

impl Clipboard for Board {
    fn set(&mut self, content: ClipboardContent) -> Result<(), ServiceError> {
        self.check("set")?;
        self.last = content.clone();
        self.memory.set(content)
    }

    fn clear(&mut self) -> Result<(), ServiceError> {
        self.check("clear")?;
        self.last = ClipboardContent::default();
        self.memory.clear()
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, ServiceError> {
        self.check("formats")?;
        self.memory.formats()
    }

    fn text(&self) -> Result<Option<String>, ServiceError> {
        self.check("text")?;
        self.memory.text()
    }

    fn html(&self) -> Result<Option<String>, ServiceError> {
        self.check("html")?;
        self.memory.html()
    }

    fn rtf(&self) -> Result<Option<String>, ServiceError> {
        self.check("rtf")?;
        self.memory.rtf()
    }

    fn image(&self) -> Result<Option<Vec<u8>>, ServiceError> {
        self.check("image")?;
        self.memory.image()
    }

    fn files(&self) -> Result<Vec<PathBuf>, ServiceError> {
        self.check("files")?;
        self.memory.files()
    }

    fn custom(&self, name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        self.check(&format!("custom:{name}"))?;
        self.memory.custom(name)
    }
}

// ----- the controller without native windows ----------------------------------------------------

fn process(args: &Args) -> ProcessEnv {
    ProcessEnv {
        os: current_os_version(),
        env: args.environment().unwrap(),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

fn write_settings(root: &Path, settings: &str) {
    let folder = root.join("profile").join("roaming").join("SS Writer");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("settings.json"), settings).unwrap();
}

/// The controller of a started application and the means to use it like the user.
struct Rig {
    controller: Rc<Controller>,
    services: ScriptedServices,
    board: Rc<RefCell<Board>>,
    engine: MonoTextEngine,
}

struct Host<P, B> {
    prepare: Option<P>,
    body: Option<B>,
    work: PathBuf,
}

impl<P: FnOnce(&mut AppCore, &Path), B: FnOnce(&Rig)> ShellHost for Host<P, B> {
    fn run(&mut self, mut core: AppCore, _args: &Args) -> i32 {
        (self.prepare.take().expect("prepared once"))(&mut core, &self.work);
        let services = ScriptedServices::new();
        let board = Rc::new(RefCell::new(Board::default()));
        let clipboard = Rc::clone(&board) as SharedClipboard;
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let controller =
            Controller::new(core, services.dialogs(), services.shell(), clipboard, clock);
        controller.open_plan(&plan);
        let rig = Rig {
            controller,
            services,
            board,
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
            "an answer no dialog asked for"
        );
        0
    }
}

/// Runs `body` over a started application whose first window shows `files[0]` (all `files` are
/// written to its work folder first). `prepare` runs before the windows are created.
fn with_rig(
    settings: &str,
    files: &[(&str, &str)],
    prepare: impl FnOnce(&mut AppCore, &Path),
    body: impl FnOnce(&Rig),
) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    for (name, text) in files {
        std::fs::write(work.join(name), text).unwrap();
    }
    write_settings(tmp.path(), settings);
    let mut args = Args {
        paths: vec![work.join(files[0].0)],
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
            prepare: Some(prepare),
            body: Some(body),
            work,
        },
    );
    assert_eq!(code, 0);
}

fn nothing(_: &mut AppCore, _: &Path) {}

/// Makes the documents of the started application go through `fs`.
fn install(core: &mut AppCore, fs: Arc<FaultFs>) {
    let env = DocEnv {
        fs,
        ..core.docs.env().clone()
    };
    core.docs = Rc::new(Documents::new(env));
}

/// The settings with `attachments` (a JSON object) and no autosave.
fn attachments(json: &str) -> String {
    format!(r#"{{"general":{{"autosave":false}},"attachments":{json}}}"#)
}

impl Rig {
    fn window(&self) -> Rc<WindowCell> {
        Rc::clone(&self.controller.windows()[0])
    }

    fn id(&self) -> WindowId {
        self.window().id
    }

    fn active(&self) -> Rc<DocEntry> {
        let window = self.window();
        let view = window.view.borrow();
        Rc::clone(&view.active_tab().expect("a tab is open").entry)
    }

    fn text(&self) -> String {
        self.active().document().borrow().text()
    }

    /// The folder of the active file.
    fn dir(&self) -> PathBuf {
        self.active()
            .path()
            .expect("a file document")
            .parent()
            .expect("a file lies in a folder")
            .to_path_buf()
    }

    /// Replaces the text of the active document.
    fn set_text(&self, text: &str) {
        let changed = replace_text(&mut self.active().document().borrow_mut(), text).unwrap();
        assert!(changed, "the text differs");
        self.controller.refresh_all();
    }

    fn run(&self, command: CommandId) -> Dispatch {
        self.controller.run_command(self.id(), command)
    }

    /// Whether the command did something.
    fn handled(&self, command: CommandId) -> bool {
        matches!(self.run(command), Dispatch::Handled)
    }

    /// A key press of the user with the editor focused: the window content reports what it
    /// wants done and the controller does it.
    fn key(&self, key: u16, mods: Modifiers) {
        let window = self.window();
        let actions = window
            .view
            .borrow_mut()
            .event(&Event::Key(KeyEvent::press(key, mods)), &self.engine);
        for action in actions {
            self.controller.handle_action(window.id, action);
        }
    }

    fn select(&self, selection: Selection) {
        self.window()
            .view
            .borrow_mut()
            .active_editor_mut()
            .expect("an editor")
            .go_to(selection);
    }

    /// Selects the first occurrence of `needle`; returns its range.
    fn select_text(&self, needle: &str) -> (usize, usize) {
        let start = self
            .text()
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in the document"));
        let end = start + needle.len();
        self.select(Selection::new(start, end));
        (start, end)
    }

    /// Puts the caret behind the first occurrence of `needle`.
    fn caret_after(&self, needle: &str) {
        let start = self
            .text()
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in the document"));
        self.select(Selection::caret(start + needle.len()));
    }

    fn read_only(&self) {
        self.window()
            .view
            .borrow_mut()
            .active_editor_mut()
            .expect("an editor")
            .set_read_only(true);
    }

    /// Another program puts `content` on the clipboard.
    fn offer(&self, content: ClipboardContent) {
        self.board.borrow_mut().set(content).unwrap();
    }

    /// What the last successful copy put on the clipboard.
    fn copied(&self) -> ClipboardContent {
        self.board.borrow().last.clone()
    }

    /// Makes the clipboard refuse `operation` from now on.
    fn fail(&self, operation: &str) {
        self.board.borrow_mut().fail(operation);
    }

    /// The user will press OK in the next message box.
    fn answer_ok(&self) {
        self.services
            .answer(DialogKind::MessageBox, Answer::Button(MessageBoxResult::Ok));
    }

    /// The texts of the dialogs shown so far.
    fn messages(&self) -> Vec<String> {
        self.services
            .dialog_log()
            .into_iter()
            .map(|dialog| dialog.message)
            .collect()
    }
}

// ----- fixtures --------------------------------------------------------------------------------

/// A real 8 × 8 PNG whose red channel is `red` (different values give different bytes).
fn png(red: u8) -> Vec<u8> {
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(
            &[red, 30, 30, 255].repeat(64),
            8,
            8,
            ExtendedColorType::Rgba8,
        )
        .expect("png encoding");
    out
}

/// The start of a JPEG whose frame is 40 × 20 pixels: enough to be recognized and sized.
fn jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 0, 20, 0, 40, 3, 1, 0, 0,
    ]
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to a string");
    }
    out
}

/// A folder next to the document's folder, outside of it.
fn outside(dir: &Path) -> PathBuf {
    let folder = dir
        .parent()
        .expect("the document's folder has a parent")
        .join("outside");
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

/// How the editor writes the absolute `path` as a link destination.
fn link_to(path: &Path) -> String {
    let slashes = path.to_string_lossy().replace('\\', "/");
    if slashes.contains([' ', '(', ')']) {
        format!("<{slashes}>")
    } else {
        slashes
    }
}

/// The `vscode-editor-data` format of text copied in a file of the language `mode`.
fn vscode(mode: &str) -> (String, Vec<u8>) {
    (
        FORMAT_VSCODE_EDITOR_DATA.to_owned(),
        format!(r#"{{"version":1,"mode":"{mode}"}}"#).into_bytes(),
    )
}

fn files_of(paths: Vec<PathBuf>) -> ClipboardContent {
    ClipboardContent {
        files: paths,
        ..ClipboardContent::default()
    }
}

fn picture_of(png: Vec<u8>) -> ClipboardContent {
    ClipboardContent {
        image: Some(png),
        ..ClipboardContent::default()
    }
}

fn rich(text: &str, html: &str) -> ClipboardContent {
    ClipboardContent {
        text: Some(text.to_owned()),
        html: Some(html.to_owned()),
        ..ClipboardContent::default()
    }
}

// ----- copy and cut ----------------------------------------------------------------------------

#[test]
fn ctrl_c_puts_the_selection_on_the_clipboard_as_text_html_rtf_and_ss_writer_markdown() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        let (start, end) = rig.select_text("**bold** text and `code`");
        rig.key(vk::C, Modifiers::CTRL);
        let source = &NOTES[start..end];
        let copied = rig.copied();
        assert_eq!(copied.text.as_deref(), Some(source));
        let html = copied.html.as_deref().expect("HTML Format");
        assert!(
            html.contains("<strong>bold</strong> text and <code"),
            "{html}"
        );
        let rtf = copied.rtf.as_deref().expect("Rich Text Format");
        assert!(rtf.starts_with("{\\rtf1"), "{rtf}");
        assert!(rtf.contains("{\\b bold}"), "{rtf}");
        assert_eq!(
            copied.custom,
            vec![(FORMAT_MARKDOWN.to_owned(), source.as_bytes().to_vec())]
        );
        assert_eq!(copied.image, None);
        assert!(copied.files.is_empty());
        assert_eq!(
            rig.board.borrow().formats().unwrap(),
            [
                ClipboardFormat::Text,
                ClipboardFormat::Html,
                ClipboardFormat::Rtf,
                ClipboardFormat::Custom(FORMAT_MARKDOWN.to_owned()),
            ]
        );
        assert_eq!(rig.text(), NOTES, "a copy leaves the document alone");
    });
}

#[test]
fn ctrl_x_copies_like_ctrl_c_and_removes_the_selection_in_one_undo_step() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        let selected = "Some **bold** text and `code`.";
        rig.select_text(selected);
        rig.key(vk::X, Modifiers::CTRL);
        let copied = rig.copied();
        assert_eq!(copied.text.as_deref(), Some(selected));
        assert!(copied.html.is_some() && copied.rtf.is_some());
        assert_eq!(
            copied.custom,
            vec![(FORMAT_MARKDOWN.to_owned(), selected.as_bytes().to_vec())]
        );
        assert_eq!(rig.text(), NOTES.replace(selected, ""));
        assert!(rig.handled(CommandId::EditUndo));
        assert_eq!(rig.text(), NOTES, "the cut is undone in one step");
    });
}

#[test]
fn copy_as_offers_each_form_alone() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        let source = "# Notes\n\nSome **bold** text and `code`.";
        let plain = "Notes\n\nSome bold text and code.";
        rig.select_text(source);

        assert!(rig.handled(CommandId::EditCopyAsMarkdown));
        assert_eq!(rig.copied(), ClipboardContent::from_text(source));

        assert!(rig.handled(CommandId::EditCopyAsPlainText));
        assert_eq!(rig.copied(), ClipboardContent::from_text(plain));

        assert!(rig.handled(CommandId::EditCopyAsRichText));
        let rich = rig.copied();
        assert_eq!(rich.text.as_deref(), Some(plain));
        assert!(
            rich.html
                .as_deref()
                .is_some_and(|html| html.contains("<h1 id=\"notes\">Notes</h1>")),
            "{:?}",
            rich.html
        );
        assert!(
            rich.rtf
                .as_deref()
                .is_some_and(|rtf| rtf.starts_with("{\\rtf1")),
            "{:?}",
            rich.rtf
        );
        assert!(
            rich.custom.is_empty(),
            "only a plain copy carries SS Writer's format"
        );

        assert!(rig.handled(CommandId::EditCopyAsHtml));
        let html = rig.copied();
        assert!(
            html.html
                .as_deref()
                .is_some_and(|html| html.contains("<h1 id=\"notes\">Notes</h1>")),
            "{:?}",
            html.html
        );
        assert_eq!(
            html.text, html.html,
            "a text editor receives the markup as text"
        );
        assert_eq!(html.rtf, None);
        assert!(html.custom.is_empty());
    });
}

#[test]
fn a_copy_embeds_the_local_images_of_the_selection_and_keeps_math_and_diagrams_as_source() {
    let markdown = "![shot](shot.png)\n\n![](<my pic.png>)\n\n![enc](other%20pic.png)\n\n\
                    ![gone](missing.png)\n\n![dir](folder.png)\n\n$E=mc^2$\n\n\
                    ```mermaid\ngraph TD\n```\n";
    with_rig(NO_AUTOSAVE, &[("notes.md", markdown)], nothing, |rig| {
        let dir = rig.dir();
        let (shot, spaced, encoded) = (png(10), png(20), jpeg());
        std::fs::write(dir.join("shot.png"), &shot).unwrap();
        std::fs::write(dir.join("my pic.png"), &spaced).unwrap();
        // A tool that percent-encodes the destination; the content decides the type, not
        // the `.png` name.
        std::fs::write(dir.join("other pic.png"), &encoded).unwrap();
        std::fs::create_dir(dir.join("folder.png")).unwrap();
        rig.select(Selection::new(0, markdown.len()));
        assert!(rig.handled(CommandId::EditCopy));

        let copied = rig.copied();
        let html = copied.html.expect("HTML Format");
        for (alt, bytes, mime) in [
            ("shot", &shot, "image/png"),
            ("", &spaced, "image/png"),
            ("enc", &encoded, "image/jpeg"),
        ] {
            let image = format!("<img src=\"{}\" alt=\"{alt}\">", data_uri(bytes, mime));
            assert!(html.contains(&image), "{alt}: {html}");
        }
        assert!(
            html.contains("<img src=\"missing.png\" alt=\"gone\">"),
            "{html}"
        );
        assert!(
            html.contains("<img src=\"folder.png\" alt=\"dir\">"),
            "a folder is no picture: {html}"
        );
        assert!(
            html.contains("<code class=\"math\">E=mc^2</code>"),
            "{html}"
        );
        assert!(
            html.contains("<code class=\"language-mermaid\">graph TD\n</code>"),
            "{html}"
        );

        let rtf = copied.rtf.expect("Rich Text Format");
        let sized = format!(
            "\\pngblip\\picw8\\pich8\\picwgoal120\\pichgoal120 {}",
            hex(&shot)
        );
        assert!(rtf.contains(&sized), "{rtf}");
        assert!(rtf.contains(&hex(&spaced)), "{rtf}");
        let jpeg = format!(
            "\\jpegblip\\picw40\\pich20\\picwgoal600\\pichgoal300 {}",
            hex(&encoded)
        );
        assert!(rtf.contains(&jpeg), "{rtf}");
        assert!(
            rtf.contains("[gone]"),
            "a missing picture shows its alt text: {rtf}"
        );

        // The Markdown in SS Writer's own format is the source, pictures stay links.
        assert_eq!(
            copied.custom,
            vec![(FORMAT_MARKDOWN.to_owned(), markdown.as_bytes().to_vec())]
        );
    });
}

#[test]
fn a_document_without_a_file_embeds_no_pictures() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        // A picture of that name lies next to the first document; the new one has no folder.
        std::fs::write(rig.dir().join("pic.png"), png(1)).unwrap();
        assert!(rig.handled(CommandId::FileNew));
        assert!(rig.active().path().is_none());
        let markdown = "![x](pic.png)\n";
        rig.set_text(markdown);
        rig.select(Selection::new(0, markdown.len()));
        assert!(rig.handled(CommandId::EditCopy));
        let copied = rig.copied();
        let html = copied.html.expect("HTML Format");
        assert!(html.contains("<img src=\"pic.png\" alt=\"x\">"), "{html}");
        let rtf = copied.rtf.expect("Rich Text Format");
        assert!(rtf.contains("[x]") && !rtf.contains("\\pngblip"), "{rtf}");
    });
}

#[test]
fn documents_that_are_not_markdown_copy_their_text_only() {
    with_rig(
        NO_AUTOSAVE,
        &[("plain.txt", "alpha **beta** gamma\n")],
        nothing,
        |rig| {
            rig.select_text("**beta**");
            for command in [
                CommandId::EditCopy,
                CommandId::EditCopyAsHtml,
                CommandId::EditCopyAsRichText,
                CommandId::EditCopyAsPlainText,
                CommandId::EditCopyAsMarkdown,
            ] {
                rig.board.borrow_mut().clear().unwrap();
                assert!(rig.handled(command), "{command}");
                assert_eq!(
                    rig.copied(),
                    ClipboardContent::from_text("**beta**"),
                    "{command}"
                );
            }
            rig.key(vk::X, Modifiers::CTRL);
            assert_eq!(rig.copied(), ClipboardContent::from_text("**beta**"));
            assert_eq!(rig.text(), "alpha  gamma\n");
        },
    );
}

#[test]
fn nothing_is_copied_without_a_selection_or_when_the_clipboard_refuses() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        let kept = ClipboardContent::from_text("kept");
        rig.offer(kept.clone());
        rig.caret_after("Some");
        for command in [
            CommandId::EditCopy,
            CommandId::EditCut,
            CommandId::EditCopyAsHtml,
        ] {
            assert!(!rig.handled(command), "{command}");
        }
        assert_eq!(rig.copied(), kept);
        assert_eq!(rig.text(), NOTES);

        rig.select_text("Some");
        rig.fail("set");
        assert!(!rig.handled(CommandId::EditCut));
        assert_eq!(
            rig.text(),
            NOTES,
            "a cut that could not copy keeps the text"
        );
        assert_eq!(rig.copied(), kept);
    });
}

#[test]
fn a_read_only_editor_copies_but_neither_cuts_nor_pastes() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        rig.read_only();
        rig.select_text("Some");
        assert!(rig.handled(CommandId::EditCut));
        assert_eq!(rig.copied().text.as_deref(), Some("Some"));
        assert_eq!(rig.text(), NOTES, "the text stays");

        rig.offer(ClipboardContent::from_text("ZZZ"));
        assert!(rig.handled(CommandId::EditPaste));
        assert!(rig.handled(CommandId::EditPastePlain));
        assert_eq!(rig.text(), NOTES);
    });
}

#[test]
fn a_window_without_tabs_does_nothing_for_copy_and_paste() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        let id = rig
            .controller
            .create_window(None, Size::new(800.0, 600.0), 1.0)
            .id;
        rig.offer(ClipboardContent::from_text("text"));
        for command in [
            CommandId::EditCopy,
            CommandId::EditCut,
            CommandId::EditPaste,
            CommandId::EditPastePlain,
        ] {
            assert!(
                matches!(rig.controller.run_command(id, command), Dispatch::Unhandled),
                "{command}"
            );
        }
        assert_eq!(rig.text(), NOTES, "the other window is untouched");
    });
}

#[test]
fn commands_that_are_no_clipboard_commands_still_reach_the_editor() {
    with_rig(NO_AUTOSAVE, &[("notes.md", NOTES)], nothing, |rig| {
        rig.select_text("Some");
        assert!(rig.handled(CommandId::FormatBold));
        assert!(
            rig.text().contains("**Some** **bold** text"),
            "{}",
            rig.text()
        );
    });
}

// ----- paste: SS Writer's own copy ---------------------------------------------------------------

#[test]
fn ss_writers_own_copy_pastes_the_exact_markdown_not_the_converted_html() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(ClipboardContent {
            text: Some("rendered text".to_owned()),
            html: Some("<p>rendered <b>html</b></p>".to_owned()),
            custom: vec![(FORMAT_MARKDOWN.to_owned(), b"own *source* _kept_".to_vec())],
            ..ClipboardContent::default()
        });
        rig.caret_after("Intro: ");
        rig.key(vk::V, Modifiers::CTRL);
        assert_eq!(rig.text(), "Intro: own *source* _kept_\nEnd\n");
    });
}

#[test]
fn copying_and_pasting_inside_ss_writer_repeats_the_selection_exactly() {
    let text = "Intro:\n\nA **b** _i_ [l](https://example.com) Z\n\nEnd\n";
    with_rig(NO_AUTOSAVE, &[("notes.md", text)], nothing, |rig| {
        rig.select_text("**b** _i_ [l](https://example.com)");
        rig.key(vk::C, Modifiers::CTRL);
        rig.caret_after("End");
        rig.key(vk::V, Modifiers::CTRL);
        assert_eq!(
            rig.text(),
            "Intro:\n\nA **b** _i_ [l](https://example.com) Z\n\nEnd**b** _i_ [l](https://example.com)\n"
        );
    });
}

// ----- paste: pictures -------------------------------------------------------------------------

#[test]
fn a_clipboard_picture_is_saved_next_to_the_document_and_linked() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        let picture = png(10);
        rig.offer(picture_of(picture.clone()));
        rig.caret_after("Intro: ");
        rig.key(vk::V, Modifiers::CTRL);
        assert_eq!(rig.text(), "Intro: ![](image-1.png)\nEnd\n");
        assert_eq!(
            std::fs::read(rig.dir().join("image-1.png")).unwrap(),
            picture
        );

        rig.key(vk::V, Modifiers::CTRL);
        assert_eq!(
            rig.text(),
            "Intro: ![](image-1.png)![](image-2.png)\nEnd\n",
            "the next free number"
        );
        assert_eq!(
            std::fs::read(rig.dir().join("image-2.png")).unwrap(),
            picture
        );
    });
}

#[test]
fn a_picture_that_comes_with_text_pastes_the_text() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(ClipboardContent {
            text: Some("caption".to_owned()),
            image: Some(png(10)),
            ..ClipboardContent::default()
        });
        rig.caret_after("Intro: ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), "Intro: caption\nEnd\n");
        assert!(!rig.dir().join("image-1.png").exists());
    });
}

#[test]
fn the_attachment_settings_decide_the_folder_and_the_file_name() {
    let cases = [
        (
            r#"{"location":"assets_folder","naming_template":"shot-{n}"}"#,
            "assets",
            "shot-1.png",
            "assets/shot-1.png",
        ),
        (
            r#"{"location":"file_assets_folder"}"#,
            "notes_assets",
            "image-1.png",
            "notes_assets/image-1.png",
        ),
        (
            r#"{"location":"custom","custom_folder":"media files"}"#,
            "media files",
            "image-1.png",
            "<media files/image-1.png>",
        ),
    ];
    for (json, folder, file, link) in cases {
        with_rig(&attachments(json), &[("notes.md", INTRO)], nothing, |rig| {
            let picture = png(10);
            rig.offer(picture_of(picture.clone()));
            rig.caret_after("Intro: ");
            assert!(rig.handled(CommandId::EditPaste), "{json}");
            assert_eq!(rig.text(), format!("Intro: ![]({link})\nEnd\n"), "{json}");
            assert_eq!(
                std::fs::read(rig.dir().join(folder).join(file)).unwrap(),
                picture,
                "{json}"
            );
        });
    }
}

#[test]
fn a_picture_cannot_be_pasted_into_a_document_without_a_file() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        assert!(rig.handled(CommandId::FileNew));
        assert!(rig.active().path().is_none());
        rig.offer(picture_of(png(10)));
        rig.answer_ok();
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), "");
        let messages = rig.messages();
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(
            messages[0].starts_with("Save this document first."),
            "{messages:?}"
        );
    });
}

#[test]
fn a_blocked_attachment_folder_is_reported_and_nothing_is_pasted() {
    let settings = attachments(r#"{"location":"custom","custom_folder":"blocked"}"#);
    with_rig(
        &settings,
        &[("notes.md", INTRO), ("blocked", "not a folder")],
        nothing,
        |rig| {
            rig.offer(picture_of(png(10)));
            rig.answer_ok();
            rig.caret_after("Intro: ");
            assert!(rig.handled(CommandId::EditPaste));
            assert_eq!(rig.text(), INTRO);
            assert_eq!(rig.messages(), [FILE_ERROR]);
            assert_eq!(
                std::fs::read_to_string(rig.dir().join("blocked")).unwrap(),
                "not a folder"
            );
        },
    );
}

// ----- paste: HTML -----------------------------------------------------------------------------

#[test]
fn html_pastes_as_markdown() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(rich(
            "Hello world and link",
            "<p>Hello <b>world</b> and <a href=\"https://example.com\">link</a></p>",
        ));
        rig.caret_after("Intro: ");
        rig.key(vk::V, Modifiers::CTRL);
        assert_eq!(
            rig.text(),
            "Intro: Hello **world** and [link](https://example.com)\nEnd\n"
        );
    });
}

#[test]
fn html_without_content_falls_back_to_the_text() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(rich("fallback", "<script>x</script>"));
        rig.caret_after("Intro: ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), "Intro: fallback\nEnd\n");
    });
}

#[test]
fn html_pictures_are_saved_in_the_attachment_folder_and_their_links_rewritten() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        let (first, second) = (png(10), png(200));
        let html = format!(
            "<p>Look: <img src=\"{}\" alt=\"first\"> and <img src=\"{}\" alt=\"second\"></p>",
            data_uri(&first, "image/png"),
            data_uri(&second, "image/png")
        );
        rig.offer(rich("Look: first and second", &html));
        rig.caret_after("Intro: ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(
            rig.text(),
            "Intro: Look: ![first](image-1.png) and ![second](image-2.png)\nEnd\n"
        );
        assert_eq!(std::fs::read(rig.dir().join("image-1.png")).unwrap(), first);
        assert_eq!(
            std::fs::read(rig.dir().join("image-2.png")).unwrap(),
            second
        );
    });
}

#[test]
fn a_failed_picture_write_removes_the_pictures_saved_before_it() {
    let faults = Arc::new(FaultFs::new());
    let installed = Arc::clone(&faults);
    with_rig(
        NO_AUTOSAVE,
        &[("notes.md", INTRO)],
        move |core, _| install(core, installed),
        |rig| {
            let html = format!(
                "<p><img src=\"{}\" alt=\"a\"><img src=\"{}\" alt=\"b\"></p>",
                data_uri(&png(10), "image/png"),
                data_uri(&png(200), "image/png")
            );
            rig.offer(rich("ab", &html));
            let dir = rig.dir();
            faults.fail_for_path(
                FsOp::WriteNew,
                &dir.join("image-2.png"),
                FsFailure::DiskFull,
                1,
            );
            rig.answer_ok();
            rig.caret_after("Intro: ");
            assert!(rig.handled(CommandId::EditPaste));
            assert_eq!(rig.text(), INTRO, "nothing is pasted");
            assert!(
                !dir.join("image-1.png").exists(),
                "the first picture is removed again"
            );
            assert!(!dir.join("image-2.png").exists());
            assert_eq!(rig.messages(), [FILE_ERROR]);
        },
    );
}

// ----- paste: code -----------------------------------------------------------------------------

#[test]
fn code_from_vs_code_pastes_literally_or_as_a_fenced_block() {
    let cases = [
        (
            "rust",
            "fn main() {\n    run();\n}\n",
            "Intro: \n```\nfn main() {\n    run();\n}\n```\nEnd\n",
        ),
        ("rust", "let x = 1;\n", "Intro: let x = 1;\nEnd\n"),
        ("markdown", "**x**\n\n- a", "Intro: **x**\n\n- a\nEnd\n"),
        ("plaintext", "plain text", "Intro: plain text\nEnd\n"),
    ];
    for (mode, code, expected) in cases {
        with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
            rig.offer(ClipboardContent {
                text: Some(code.to_owned()),
                custom: vec![vscode(mode)],
                ..ClipboardContent::default()
            });
            rig.caret_after("Intro: ");
            assert!(rig.handled(CommandId::EditPaste), "{mode}: {code:?}");
            assert_eq!(rig.text(), expected, "{mode}: {code:?}");
        });
    }
}

#[test]
fn a_monospace_block_of_html_pastes_as_a_fenced_code_block() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(rich(
            "let a = 1;\nlet b = 2;",
            "<pre>let a = 1;\nlet b = 2;</pre>",
        ));
        rig.caret_after("Intro: ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(
            rig.text(),
            "Intro: \n```\nlet a = 1;\nlet b = 2;\n```\nEnd\n"
        );
    });
}

#[test]
fn inside_a_code_block_only_the_plain_text_is_pasted() {
    let text = "text\n\n```\nlet a = 1;\n```\n";
    with_rig(NO_AUTOSAVE, &[("notes.md", text)], nothing, |rig| {
        rig.offer(rich("Hello w", "<p>Hello <b>w</b></p>"));
        rig.caret_after("let ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), "text\n\n```\nlet Hello wa = 1;\n```\n");

        // A picture has no text: nothing is pasted and no file is written.
        rig.offer(picture_of(png(1)));
        assert!(!rig.handled(CommandId::EditPaste));
        assert!(!rig.dir().join("image-1.png").exists());
    });
}

// ----- paste: text -----------------------------------------------------------------------------

#[test]
fn a_url_over_a_selection_becomes_a_link() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(ClipboardContent::from_text("https://example.com/docs"));
        rig.select_text("Intro");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), "[Intro](https://example.com/docs): \nEnd\n");
    });
}

#[test]
fn ctrl_shift_v_pastes_the_plain_text_whatever_else_the_clipboard_holds() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.offer(ClipboardContent {
            text: Some("https://example.com/p".to_owned()),
            html: Some("<p><a href=\"https://example.com/p\">link</a></p>".to_owned()),
            custom: vec![(FORMAT_MARKDOWN.to_owned(), b"own **md**".to_vec())],
            ..ClipboardContent::default()
        });
        rig.select_text("Intro");
        rig.key(vk::V, Modifiers::CTRL | Modifiers::SHIFT);
        assert_eq!(
            rig.text(),
            "https://example.com/p: \nEnd\n",
            "no link, no own Markdown"
        );
    });
}

#[test]
fn documents_that_are_not_markdown_paste_plain_text() {
    with_rig(
        NO_AUTOSAVE,
        &[("plain.txt", "alpha beta\n")],
        nothing,
        |rig| {
            rig.offer(rich("X", "<p><b>Y</b></p>"));
            rig.caret_after("alpha ");
            assert!(rig.handled(CommandId::EditPaste));
            assert_eq!(rig.text(), "alpha Xbeta\n");

            rig.offer(picture_of(png(1)));
            assert!(!rig.handled(CommandId::EditPaste));
            assert_eq!(rig.text(), "alpha Xbeta\n");
            assert!(!rig.dir().join("image-1.png").exists());
        },
    );
}

#[test]
fn nothing_is_pasted_from_an_empty_or_unreadable_clipboard() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        rig.caret_after("Intro: ");
        assert!(!rig.handled(CommandId::EditPaste), "an empty clipboard");
        rig.offer(ClipboardContent {
            rtf: Some("{\\rtf1 rich}".to_owned()),
            ..ClipboardContent::default()
        });
        assert!(
            !rig.handled(CommandId::EditPaste),
            "RTF alone is not pasted"
        );
        rig.offer(ClipboardContent::from_text("hidden"));
        rig.fail("formats");
        assert!(
            !rig.handled(CommandId::EditPaste),
            "an unreadable clipboard"
        );
        assert_eq!(rig.text(), INTRO);
    });
}

// ----- paste: files ----------------------------------------------------------------------------

#[test]
fn pasted_files_are_linked_where_they_lie_and_outside_pictures_are_copied_in() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        let dir = rig.dir();
        let outside = outside(&dir);
        std::fs::create_dir_all(dir.join("pics")).unwrap();
        let inside = dir.join("pics").join("a.png");
        let far = outside.join("b.png");
        let report = outside.join("report.pdf");
        std::fs::write(&inside, png(1)).unwrap();
        std::fs::write(&far, png(2)).unwrap();
        std::fs::write(&report, b"%PDF").unwrap();
        rig.offer(files_of(vec![inside, far.clone(), report]));
        rig.caret_after("Intro: ");
        rig.key(vk::V, Modifiers::CTRL);
        assert_eq!(
            rig.text(),
            "Intro: ![](pics/a.png)\n\n![](b.png)\n\n[report.pdf](../outside/report.pdf)\nEnd\n"
        );
        assert_eq!(std::fs::read(dir.join("b.png")).unwrap(), png(2));
        assert!(
            !dir.join("a.png").exists(),
            "a picture inside the folder is not copied"
        );
        assert!(far.exists(), "the original stays");

        // The same picture again gets a name of its own.
        rig.offer(files_of(vec![far]));
        assert!(rig.handled(CommandId::EditPaste));
        assert!(
            rig.text().ends_with("![](<b (2).png>)\nEnd\n"),
            "{}",
            rig.text()
        );
        assert_eq!(std::fs::read(dir.join("b (2).png")).unwrap(), png(2));
    });
}

#[test]
fn pictures_inside_the_workspace_are_linked_in_place() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        let dir = rig.dir();
        let shared = outside(&dir).join("c.png");
        std::fs::write(&shared, png(3)).unwrap();
        let root = dir.parent().expect("a parent folder").to_path_buf();
        rig.controller.show_workspace(&rig.window(), &root);
        rig.offer(files_of(vec![shared]));
        rig.caret_after("Intro: ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), "Intro: ![](../outside/c.png)\nEnd\n");
        assert!(!dir.join("c.png").exists(), "nothing is copied");
    });
}

#[test]
fn files_pasted_into_a_document_without_a_file_are_linked_by_their_absolute_path() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        let outside = outside(&rig.dir());
        let picture = outside.join("d.png");
        let report = outside.join("report.pdf");
        std::fs::write(&picture, png(4)).unwrap();
        std::fs::write(&report, b"%PDF").unwrap();
        assert!(rig.handled(CommandId::FileNew));
        assert!(rig.active().path().is_none());
        rig.offer(files_of(vec![picture.clone(), report.clone()]));
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(
            rig.text(),
            format!(
                "![]({})\n\n[report.pdf]({})",
                link_to(&picture),
                link_to(&report)
            )
        );
    });
}

#[test]
fn a_file_that_cannot_be_copied_is_reported_and_the_copies_made_before_it_are_removed() {
    with_rig(NO_AUTOSAVE, &[("notes.md", INTRO)], nothing, |rig| {
        let dir = rig.dir();
        let outside = outside(&dir);
        let far = outside.join("b.png");
        std::fs::write(&far, png(2)).unwrap();
        rig.offer(files_of(vec![far.clone(), outside.join("missing.png")]));
        rig.answer_ok();
        rig.caret_after("Intro: ");
        assert!(rig.handled(CommandId::EditPaste));
        assert_eq!(rig.text(), INTRO, "nothing is pasted");
        assert!(
            !dir.join("b.png").exists(),
            "the first copy is removed again"
        );
        assert!(far.exists());
        assert_eq!(rig.messages(), [FILE_ERROR]);
    });
}

// ----- the key table and the copy kinds --------------------------------------------------------

#[test]
fn key_command_recognizes_exactly_ctrl_c_ctrl_x_ctrl_v_and_ctrl_shift_v_presses() {
    let ctrl = Modifiers::CTRL;
    let released = KeyEvent {
        is_down: false,
        ..KeyEvent::press(vk::C, ctrl)
    };
    let table = [
        (KeyEvent::press(vk::C, ctrl), Some(CommandId::EditCopy)),
        (KeyEvent::press(vk::X, ctrl), Some(CommandId::EditCut)),
        (KeyEvent::press(vk::V, ctrl), Some(CommandId::EditPaste)),
        (
            KeyEvent::press(vk::V, ctrl | Modifiers::SHIFT),
            Some(CommandId::EditPastePlain),
        ),
        (KeyEvent::press(vk::C, ctrl | Modifiers::SHIFT), None),
        (KeyEvent::press(vk::X, ctrl | Modifiers::SHIFT), None),
        (KeyEvent::press(vk::Z, ctrl), None),
        (KeyEvent::press(vk::C, Modifiers::NONE), None),
        (KeyEvent::press(vk::C, Modifiers::SHIFT), None),
        // Ctrl+Alt is how AltGr arrives: it types characters, it does not copy.
        (KeyEvent::press(vk::C, ctrl | Modifiers::ALT), None),
        (KeyEvent::press(vk::V, ctrl | Modifiers::WIN), None),
        (released, None),
    ];
    for (key, expected) in table {
        assert_eq!(key_command(&key), expected, "{key:?}");
    }
}

#[test]
fn copy_kind_maps_the_copy_commands_and_nothing_else() {
    let table = [
        (CommandId::EditCopy, Some(CopyKind::Everything)),
        (CommandId::EditCut, Some(CopyKind::Everything)),
        (CommandId::EditCopyAsPlainText, Some(CopyKind::PlainText)),
        (CommandId::EditCopyAsMarkdown, Some(CopyKind::Markdown)),
        (CommandId::EditCopyAsRichText, Some(CopyKind::RichText)),
        (CommandId::EditCopyAsHtml, Some(CopyKind::Html)),
        (CommandId::EditPaste, None),
        (CommandId::FormatBold, None),
        (CommandId::FileSave, None),
    ];
    for (command, expected) in table {
        assert_eq!(copy_kind(command), expected, "{command}");
    }
}

// ----- plan_paste ------------------------------------------------------------------------------

fn plan(content: ClipboardContent) -> Paste {
    plan_paste(&Board::with(content)).expect("the clipboard can be read")
}

const RICH_HTML: &str =
    "<h2>Title <b>bold</b></h2><p>a <i>b</i> <u>c</u> <s>d</s> <mark>e</mark> <code>f</code></p>";
const RICH_MARKDOWN: &str = "## Title bold\n\na *b* ~c~ ~~d~~ ==e== `f`";

#[test]
fn plan_paste_prefers_own_markdown_pictures_files_code_html_and_text_in_that_order() {
    let own = |bytes: &[u8]| (FORMAT_MARKDOWN.to_owned(), bytes.to_vec());
    let markdown = |text: &str| Paste::Markdown {
        markdown: text.to_owned(),
        images: Vec::new(),
    };
    let picture = png(5);
    let paths = vec![PathBuf::from(r"C:\x\a.png")];

    assert_eq!(plan(ClipboardContent::default()), Paste::Nothing);
    assert_eq!(plan(ClipboardContent::from_text("t")), Paste::Text);

    // SS Writer's own format beats everything else on the clipboard.
    let mut everything = rich("t", "<p>h</p>");
    everything.image = Some(picture.clone());
    everything.files = paths.clone();
    everything.custom = vec![own(b"own **md**"), vscode("rust")];
    assert_eq!(plan(everything), markdown("own **md**"));
    assert_eq!(
        plan(ClipboardContent {
            custom: vec![own(b"a\xFFb")],
            ..ClipboardContent::default()
        }),
        markdown("a\u{FFFD}b"),
        "bytes that are not UTF-8 are replaced"
    );

    // A picture alone is a screenshot; with text or files it is a by-product.
    assert_eq!(
        plan(picture_of(picture.clone())),
        Paste::Image(picture.clone())
    );
    assert_eq!(
        plan(ClipboardContent {
            text: Some("t".to_owned()),
            image: Some(picture.clone()),
            ..ClipboardContent::default()
        }),
        Paste::Text
    );
    assert_eq!(
        plan(ClipboardContent {
            image: Some(picture),
            files: paths.clone(),
            ..ClipboardContent::default()
        }),
        Paste::Files(paths.clone())
    );

    // Files beat text.
    assert_eq!(
        plan(ClipboardContent {
            text: Some("a.png".to_owned()),
            files: paths.clone(),
            ..ClipboardContent::default()
        }),
        Paste::Files(paths)
    );

    // VS Code: code, unless the file it came from was Markdown or plain text.
    let vscode_text = |mode: &str, text: &str| ClipboardContent {
        text: Some(text.to_owned()),
        custom: vec![vscode(mode)],
        ..ClipboardContent::default()
    };
    assert_eq!(
        plan(vscode_text("rust", "let x")),
        Paste::Code("let x".to_owned())
    );
    assert_eq!(plan(vscode_text("markdown", "**x**")), Paste::Text);
    assert_eq!(plan(vscode_text("plaintext", "x")), Paste::Text);
    assert_eq!(
        plan(ClipboardContent {
            text: Some("x".to_owned()),
            custom: vec![(FORMAT_VSCODE_EDITOR_DATA.to_owned(), b"{}".to_vec())],
            ..ClipboardContent::default()
        }),
        Paste::Code("x".to_owned()),
        "no language mode: code"
    );
    let mut with_html = rich("let x", "<p><b>h</b></p>");
    with_html.custom = vec![vscode("rust")];
    assert_eq!(
        plan(with_html),
        Paste::Code("let x".to_owned()),
        "the text of VS Code beats its HTML"
    );
    assert_eq!(
        plan(ClipboardContent {
            html: Some("<p>a</p><p>b</p>".to_owned()),
            custom: vec![vscode("rust")],
            ..ClipboardContent::default()
        }),
        Paste::Code("a\nb".to_owned()),
        "VS Code HTML without text is code"
    );

    // HTML: a lone monospace block is code, anything else is converted.
    assert_eq!(
        plan(rich("x\ny", "<pre>x\ny</pre>")),
        Paste::Code("x\ny".to_owned())
    );
    assert_eq!(plan(rich("t", RICH_HTML)), markdown(RICH_MARKDOWN));
    assert_eq!(
        plan(rich("t", "<p> </p>")),
        Paste::Text,
        "HTML without content falls back to the text"
    );
    assert_eq!(
        plan(ClipboardContent {
            html: Some("<p> </p>".to_owned()),
            ..ClipboardContent::default()
        }),
        Paste::Nothing
    );
    assert_eq!(
        plan(ClipboardContent {
            text: Some("t".to_owned()),
            custom: vec![("other format".to_owned(), vec![1])],
            ..ClipboardContent::default()
        }),
        Paste::Text,
        "unknown formats are ignored"
    );
}

#[test]
fn plan_paste_hands_pictures_embedded_in_html_over_with_their_bytes() {
    let html = "<p><img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"dot\"></p>";
    assert_eq!(
        plan(rich("dot", html)),
        Paste::Markdown {
            markdown: "![dot](ssw-paste-assets/image-1.png)".to_owned(),
            images: vec![Asset {
                relative_path: "ssw-paste-assets/image-1.png".to_owned(),
                bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
            }],
        }
    );
}

#[test]
fn plan_paste_reports_a_clipboard_that_cannot_be_read() {
    let refused: Result<Paste, ServiceError> = Err(ServiceError::Unavailable {
        service: "clipboard",
    });
    let code = ClipboardContent {
        text: Some("let x".to_owned()),
        custom: vec![vscode("rust")],
        ..ClipboardContent::default()
    };
    let own = ClipboardContent {
        text: Some("t".to_owned()),
        custom: vec![(FORMAT_MARKDOWN.to_owned(), b"own".to_vec())],
        ..ClipboardContent::default()
    };
    let cases = [
        ("formats", ClipboardContent::from_text("t"), "formats"),
        ("own format", own, "custom:SS Writer Markdown"),
        ("picture", picture_of(png(1)), "image"),
        ("files", files_of(vec![PathBuf::from(r"C:\x")]), "files"),
        ("VS Code text", code.clone(), "text"),
        ("VS Code data", code, "custom:vscode-editor-data"),
        ("html", rich("t", "<p>h</p>"), "html"),
    ];
    for (what, content, operation) in cases {
        let mut board = Board::with(content);
        board.fail(operation);
        assert_eq!(plan_paste(&board), refused, "{what}");
    }
}

// ----- attachment folders and destinations -----------------------------------------------------

fn attachment_settings(location: AttachmentLocation, custom_folder: &str) -> Attachments {
    Attachments {
        location,
        custom_folder: custom_folder.to_owned(),
        ..Attachments::default()
    }
}

#[test]
fn the_attachment_folder_follows_the_location_setting() {
    let document = Path::new(r"C:\work\notes.md");
    let table = [
        (AttachmentLocation::NextToFile, "", r"C:\work"),
        (AttachmentLocation::AssetsFolder, "", r"C:\work\assets"),
        (
            AttachmentLocation::FileAssetsFolder,
            "",
            r"C:\work\notes_assets",
        ),
        (AttachmentLocation::Custom, "", r"C:\work"),
        (AttachmentLocation::Custom, "  ", r"C:\work"),
        (
            AttachmentLocation::Custom,
            " media\\pics ",
            r"C:\work\media\pics",
        ),
        (AttachmentLocation::Custom, r"D:\shared", r"D:\shared"),
    ];
    for (location, custom, folder) in table {
        let target = AttachmentTarget::new(document, &attachment_settings(location, custom));
        assert_eq!(
            target.folder(),
            Path::new(folder),
            "{location:?} {custom:?}"
        );
        assert_eq!(target.doc_dir(), Path::new(r"C:\work"));
    }
}

#[test]
fn destinations_are_relative_with_slashes_or_absolute_when_there_is_no_relative_path() {
    let target = AttachmentTarget::new(
        Path::new(r"C:\work\notes.md"),
        &attachment_settings(AttachmentLocation::AssetsFolder, ""),
    );
    let table = [
        (r"C:\work\assets\a.png", "assets/a.png"),
        (r"C:\work\b.png", "b.png"),
        (r"C:\shared\c.png", "../shared/c.png"),
        (r"D:\shared\d.png", "D:/shared/d.png"),
    ];
    for (file, expected) in table {
        assert_eq!(target.destination(Path::new(file)), expected, "{file}");
    }
}

#[test]
fn relative_destination_climbs_out_of_the_base_and_ignores_the_case_of_the_folders() {
    let table = [
        (r"C:\a\b", r"C:\a\b\c\d.png", Some("c/d.png")),
        (r"C:\a\b", r"C:\a\x\d.png", Some("../x/d.png")),
        (r"C:\a\b\c", r"C:\a\d.png", Some("../../d.png")),
        (r"C:\a\b", r"C:\a", Some("..")),
        (r"C:\a\b", r"c:\A\B\Pic.png", Some("Pic.png")),
        (r"C:\a\b", r"D:\a\b\c.png", None),
        (r"C:\a\b", r"C:\a\b", None),
        ("", r"C:\a", None),
    ];
    for (base, target, expected) in table {
        assert_eq!(
            relative_destination(Path::new(base), Path::new(target)).as_deref(),
            expected,
            "{base} -> {target}"
        );
    }
}

// ----- numbered names and copies ---------------------------------------------------------------

fn target_in(dir: &Path, location: AttachmentLocation) -> AttachmentTarget {
    AttachmentTarget::new(&dir.join("notes.md"), &attachment_settings(location, ""))
}

#[test]
fn save_numbered_creates_the_folder_and_takes_the_lowest_free_number() {
    let tmp = tempfile::tempdir().unwrap();
    let target = target_in(tmp.path(), AttachmentLocation::AssetsFolder);
    let folder = tmp.path().join("assets");
    let first = save_numbered(&RealFs, &target, "image-{n}", "png", b"one").unwrap();
    let second = save_numbered(&RealFs, &target, "image-{n}", "png", b"two").unwrap();
    assert_eq!(first, folder.join("image-1.png"));
    assert_eq!(second, folder.join("image-2.png"));
    assert_eq!(std::fs::read(&first).unwrap(), b"one");
    assert_eq!(std::fs::read(&second).unwrap(), b"two");

    std::fs::remove_file(&first).unwrap();
    let third = save_numbered(&RealFs, &target, "image-{n}", "png", b"three").unwrap();
    assert_eq!(third, first, "a freed number is used again");
    assert_eq!(std::fs::read(&third).unwrap(), b"three");
}

#[test]
fn save_numbered_names_files_by_the_template_and_falls_back_to_image_n() {
    let too_long = format!("{}{{n}}", "x".repeat(300));
    let cases = [
        ("shot {n} final", "shot 1 final.jpg"),
        ("no number", "image-1.jpg"),
        ("a:{n}", "image-1.jpg"),
        (too_long.as_str(), "image-1.jpg"),
    ];
    for (template, expected) in cases {
        let tmp = tempfile::tempdir().unwrap();
        let target = target_in(tmp.path(), AttachmentLocation::NextToFile);
        let saved = save_numbered(&RealFs, &target, template, "jpg", b"x").unwrap();
        assert_eq!(saved, tmp.path().join(expected), "{template:?}");
    }
}

#[test]
fn save_numbered_skips_a_name_taken_in_the_meantime_and_reports_every_other_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let target = target_in(tmp.path(), AttachmentLocation::AssetsFolder);
    let folder = tmp.path().join("assets");

    // Another program creates image-1.png between the check and the write.
    let race = FaultFs::new();
    race.fail_for_path(
        FsOp::WriteNew,
        &folder.join("image-1.png"),
        FsFailure::Code(win32::FILE_EXISTS),
        1,
    );
    let saved = save_numbered(&race, &target, "image-{n}", "png", b"x").unwrap();
    assert_eq!(saved, folder.join("image-2.png"));

    let full = FaultFs::new();
    full.fail(FsOp::WriteNew, FsFailure::DiskFull, 1);
    assert!(matches!(
        save_numbered(&full, &target, "image-{n}", "png", b"x"),
        Err(FsError::DiskFull(_))
    ));
    let unreadable = FaultFs::new();
    unreadable.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    assert!(matches!(
        save_numbered(&unreadable, &target, "image-{n}", "png", b"x"),
        Err(FsError::AccessDenied(_))
    ));
    let no_folder = FaultFs::new();
    no_folder.fail(FsOp::CreateDirAll, FsFailure::AccessDenied, 1);
    assert!(matches!(
        save_numbered(&no_folder, &target, "image-{n}", "png", b"x"),
        Err(FsError::AccessDenied(_))
    ));
    assert!(
        !folder.join("image-1.png").exists(),
        "a failed save leaves no file"
    );
}

#[test]
fn copy_to_folder_keeps_the_name_and_never_overwrites() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("photo.png");
    std::fs::write(&source, b"pixels").unwrap();
    let folder = tmp.path().join("deep").join("media");
    let first = copy_to_folder(&RealFs, &folder, &source).unwrap();
    let second = copy_to_folder(&RealFs, &folder, &source).unwrap();
    assert_eq!(first, folder.join("photo.png"));
    assert_eq!(second, folder.join("photo (2).png"));
    assert_eq!(std::fs::read(&first).unwrap(), b"pixels");
    assert_eq!(std::fs::read(&second).unwrap(), b"pixels");

    let bare = tmp.path().join("README");
    std::fs::write(&bare, b"read me").unwrap();
    assert_eq!(
        copy_to_folder(&RealFs, &folder, &bare).unwrap(),
        folder.join("README")
    );
    assert_eq!(
        copy_to_folder(&RealFs, &folder, &bare).unwrap(),
        folder.join("README (2)")
    );
}

#[test]
fn copy_to_folder_reports_a_missing_source_and_every_failing_file_call() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("photo.png");
    std::fs::write(&source, b"pixels").unwrap();
    let folder = tmp.path().join("media");

    let missing = tmp.path().join("gone.png");
    assert!(matches!(
        copy_to_folder(&RealFs, &folder, &missing),
        Err(FsError::NotFound(_))
    ));
    assert!(!folder.join("gone.png").exists());

    let no_folder = FaultFs::new();
    no_folder.fail(FsOp::CreateDirAll, FsFailure::AccessDenied, 1);
    assert!(matches!(
        copy_to_folder(&no_folder, &folder, &source),
        Err(FsError::AccessDenied(_))
    ));
    let unreadable = FaultFs::new();
    unreadable.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    assert!(matches!(
        copy_to_folder(&unreadable, &folder, &source),
        Err(FsError::AccessDenied(_))
    ));
}

// ----- local images ----------------------------------------------------------------------------

#[test]
fn local_images_are_found_by_raw_or_percent_encoded_name_and_typed_by_content_first() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    std::fs::write(dir.join("a.png"), jpeg()).unwrap();
    std::fs::write(dir.join("b.png"), b"not an image").unwrap();
    std::fs::write(dir.join("c.dat"), b"not an image").unwrap();
    std::fs::write(dir.join("d e.png"), png(1)).unwrap();
    std::fs::create_dir(dir.join("dir.png")).unwrap();

    let images = LocalImages::new(&RealFs, Some(dir));
    assert_eq!(images.image("a.png"), Some((jpeg(), "image/jpeg")));
    assert_eq!(
        images.image("b.png"),
        Some((b"not an image".to_vec(), "image/png")),
        "unrecognized content takes the type of the name"
    );
    assert_eq!(images.image("c.dat"), None);
    assert_eq!(images.image("d e.png"), Some((png(1), "image/png")));
    assert_eq!(images.image("d%20e.png"), Some((png(1), "image/png")));
    assert_eq!(images.image("dir.png"), None, "a folder is no picture");
    assert_eq!(images.image("missing.png"), None);
    assert_eq!(
        LocalImages::new(&RealFs, None).image("a.png"),
        None,
        "a document without a file has no folder"
    );
}

#[test]
fn local_images_respect_the_size_limits_and_survive_a_failing_read() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let signature = b"\x89PNG\r\n\x1a\n".to_vec();
    std::fs::write(dir.join("p1.png"), &signature).unwrap();
    std::fs::write(dir.join("p2.png"), &signature).unwrap();
    std::fs::write(dir.join("p3.png"), &signature[..7]).unwrap();
    let mut big = signature.clone();
    big.extend_from_slice(&[0; 8]);
    std::fs::write(dir.join("big.png"), &big).unwrap();

    // 10 bytes per picture, 15 bytes together.
    let images = LocalImages::with_limits(&RealFs, Some(dir), 10, 15);
    assert_eq!(images.image("big.png"), None, "above the per-picture limit");
    assert!(images.image("p1.png").is_some());
    assert_eq!(images.image("p2.png"), None, "8 of the remaining 7 bytes");
    assert!(
        images.image("p3.png").is_some(),
        "7 of the remaining 7 bytes"
    );
    assert_eq!(images.image("p1.png"), None, "the budget is used up");

    let faults = FaultFs::new();
    faults.fail(FsOp::Read, FsFailure::AccessDenied, 2);
    let images = LocalImages::new(&faults, Some(dir));
    assert_eq!(images.image("p1.png"), None, "both attempts to read fail");
    assert!(images.image("p1.png").is_some(), "the next read works");
}
