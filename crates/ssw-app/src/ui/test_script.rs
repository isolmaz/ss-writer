//! The scripted end-to-end harness (`SSWriter.exe --test-hidden --test-profile <dir>
//! --test-script <file>`).
//!
//! A script is a list of lines, each one user action or one assertion, run against the real
//! [`Controller`]: real documents, real files, the real shortcut router, the real editor input
//! path and the real command dispatch. Only what would put something on the user's screen is
//! replaced (see [`super::scripted_services`]); no window exists, so nothing is ever shown,
//! activated or focused. The process exits with `0` when every step passed and with a non-zero
//! code and a message naming the line of the first failing step otherwise.
//!
//! After every step the harness does what the message loop does: it carries out the requests
//! the controller queued, runs one housekeeping turn of the application core (worker results,
//! external file changes, autosave) and fails the script when a dialog appeared that no
//! `answer`/`save-as` step prepared. At the end the session is stored like a clean exit, so a
//! second launch with the same profile restores it.
//!
//! # Language
//!
//! `#` starts a comment line. Paths and words may be written `"in quotes"` (`\"` is a quote,
//! nothing else is escaped). The text of `type`, `paste-text`, `select-text`, `clipboard-text`,
//! `assert-text-is` and `assert-dialog` is the rest of the line; there `\n`, `\t`, `\r` and
//! `\\` are escapes. Offsets are UTF-8 byte offsets.
//!
//! | Step | Does |
//! |---|---|
//! | `open <path>` | opens the file in the active window (command line / Explorer open) |
//! | `new` | `file.new` |
//! | `command <id>` | runs the command (`format.bold`); fails when nobody handles it |
//! | `type <text>` | one text event per character; `\n` presses Enter, `\t` presses Tab |
//! | `key <chord>` | a key press through the shortcut router (`ctrl+b`, `enter`, `shift+tab`) |
//! | `select <anchor> <head>` / `select-text <text>` | sets the selection (first occurrence) |
//! | `save` / `save-as <path>` | `file.save`; `file.save_as` answered with `<path>` |
//! | `answer <kind> <choice…>` | queues the answer of the next dialog: kind `open-file`, `save-file`, `pick-folder` (paths or `cancel`), `message-box` (`ok`, `cancel`, `yes`, `no`, `save`, `dont-save`) |
//! | `clipboard-text <text>`, `clipboard-html <file>`, `clipboard-image <png>` | prepares the clipboard content |
//! | `paste` / `paste-plain` | puts the prepared content on the clipboard, `edit.paste` / `edit.paste_plain` |
//! | `paste-text <text>`, `paste-html <file>`, `paste-image <png>` | `clipboard-…` then `paste` |
//! | `tab <n>` | activates the n-th tab (from 1) of the active window |
//! | `advance <ms>` | moves the editor's clock (typing groups undo steps after 1 s of pause) |
//! | `sleep <ms>` | waits in real time (autosave delays) |
//! | `wait-idle` | waits until the workers have delivered everything |
//! | `external-write <path> <text>` / `wait-external [ms]` | another program writes a file / waits for the application to notice a change |
//! | `assert-text <file>` / `assert-text-is <text>` | the active document's text equals the file / the text |
//! | `assert-file <path> <expected-file>` | the bytes of `<path>` equal the bytes of the other file |
//! | `assert-exists <path>` / `assert-no-file <path>` | existence |
//! | `assert-magic <path> <kind>` | the file looks like `zip`, `docx`, `odt`, `epub`, `pdf`, `rtf`, `html` or `png` |
//! | `assert-dirty true\|false`, `assert-tabs <n>`, `assert-selection <anchor> <head>` | state of the active window |
//! | `assert-clipboard <file>` | the clipboard's text equals the file |
//! | `assert-dialog <kind> <text>` | a dialog of the kind showed a title or message containing the text |
//! | `dump-state <json-path>` | writes windows, tabs, documents, banners, dialogs, shell requests |
//! | `theme light\|dark` | overrides the colors of every window (never touches the OS theme) |
//! | `screenshot <png> [width height] [scale]` | paints the active window (overlays and popups included) offscreen into a PNG: DIPs (default 1440 900), scale 0.5 to 4 (default 1; 2 is hi-DPI) |
//! | `settings-page <page>` | shows a page of the open Settings window (`general`, `appearance`, `editor`, `sidebar`, `attachments`, `export`, `shortcuts`, `advanced`) |
//! | `screenshot-settings <png> [width height] [scale]` | the same for the Settings window (default 900 640); open it with `command settings.open` |
//! | `quit` | ends the script (a clean exit) |
//!
//! `theme` and the screenshot steps switch the harness to the real DirectWrite text engine and
//! give the editors the block renderers of the window (tables, callouts, math, diagrams,
//! images), so the pixels are what a window shows; a script without them keeps the headless
//! monospace engine. Mica does not exist offscreen: the solid fallback is painted. A script
//! that uses them needs COM on its thread ([`ScriptHost`] provides it).

use std::cell::RefCell;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::keymap::KeyChord;
use ssw_core::settings::SettingsSection;
use ssw_ui::canvas::Canvas;
use ssw_ui::geometry::Color;
use ssw_ui::input::{Event, KeyEvent, Modifiers, TextInput, vk};
use ssw_ui::services::{
    Clipboard, ClipboardContent, Clock, FakeClock, MemoryClipboard, MessageBoxResult,
};
use ssw_ui::shell::tabs::TabsEvent;
use ssw_ui::shell::titlebar::TitleEvent;
use ssw_ui::shortcuts::{CharProbe, ShortcutOutcome};
use ssw_ui::text::{MonoTextEngine, TextLayoutEngine};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widgets::SharedClipboard;
use ssw_ui::widgets::anim::Motion;
use ssw_win::chrome::{ChromeState, paint_caption_buttons};

use super::controller::{Controller, Request, WindowCell};
use super::screenshot::{Geometry, Visuals};
use super::scripted_services::{Answer, DialogKind, ScriptedServices};
use super::settings_window::{SettingsCell, SettingsSetup, WINDOW_SIZE, WindowsKeyNames};
use super::shell_view::{ShellAction, TabView};
use crate::app::{AppCore, PumpReport, ShellHost};
use crate::args::Args;
use crate::commands::Dispatch;

/// Exit code when the script file cannot be read (or none was given).
pub const EXIT_SCRIPT_UNREADABLE: i32 = 6;
/// Exit code when the script has a syntax error.
pub const EXIT_SCRIPT_INVALID: i32 = 7;
/// Exit code when a step failed.
pub const EXIT_SCRIPT_FAILED: i32 = 8;

/// How long `wait-idle` waits for the workers by default.
const DEFAULT_WAIT_LIMIT: Duration = Duration::from_secs(30);
/// How long `wait-external` waits when the step names no limit.
const DEFAULT_EXTERNAL_WAIT: Duration = Duration::from_secs(10);
/// Pause between housekeeping turns while waiting.
const WAIT_POLL: Duration = Duration::from_millis(5);

/// Why a script did not run to its end.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    /// A line is not valid.
    #[error("line {line}: {message}")]
    Syntax {
        /// The 1-based line.
        line: usize,
        /// What is wrong.
        message: String,
    },
    /// A step failed (an assertion, a command nobody handled, an unexpected dialog). Line `0`
    /// stands for the start of the application, before the first step.
    #[error("line {line}: {message}")]
    Failed {
        /// The 1-based line of the step.
        line: usize,
        /// What went wrong.
        message: String,
    },
}

/// What `assert-magic` recognizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Magic {
    /// Any zip archive.
    Zip,
    /// A Word document: a zip with `word/document.xml`.
    Docx,
    /// An OpenDocument text: a zip with the text mimetype.
    Odt,
    /// An ePub: a zip with the ePub mimetype.
    Epub,
    /// A PDF file.
    Pdf,
    /// An RTF file.
    Rtf,
    /// An HTML document.
    Html,
    /// A PNG image.
    Png,
}

impl Magic {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "zip" => Self::Zip,
            "docx" => Self::Docx,
            "odt" => Self::Odt,
            "epub" => Self::Epub,
            "pdf" => Self::Pdf,
            "rtf" => Self::Rtf,
            "html" => Self::Html,
            "png" => Self::Png,
            _ => return None,
        })
    }

    fn matches(self, bytes: &[u8]) -> bool {
        let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|window| window == needle);
        let zip = bytes.starts_with(b"PK\x03\x04");
        match self {
            Self::Zip => zip,
            Self::Docx => zip && contains(b"word/document.xml"),
            Self::Odt => zip && contains(b"application/vnd.oasis.opendocument.text"),
            Self::Epub => zip && contains(b"application/epub+zip"),
            Self::Pdf => bytes.starts_with(b"%PDF-"),
            Self::Rtf => bytes.starts_with(b"{\\rtf"),
            Self::Html => {
                let head = String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]).to_lowercase();
                head.contains("<!doctype html") || head.contains("<html")
            }
            Self::Png => bytes.starts_with(&[0x89, b'P', b'N', b'G']),
        }
    }
}

/// One step of a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `open <path>`.
    Open(PathBuf),
    /// `command <id>` (and the aliases `new`, `save`).
    Command(CommandId),
    /// `type <text>`.
    Type(String),
    /// `key <chord>`.
    Key(KeyChord),
    /// `select <anchor> <head>`.
    Select {
        /// Where the selection started.
        anchor: usize,
        /// The caret.
        head: usize,
    },
    /// `select-text <text>`.
    SelectText(String),
    /// `save-as <path>`.
    SaveAs(PathBuf),
    /// `answer <kind> <choice…>`.
    Answer(DialogKind, Answer),
    /// `clipboard-text <text>`.
    ClipboardText(String),
    /// `clipboard-html <file>`.
    ClipboardHtml(PathBuf),
    /// `clipboard-image <png>`.
    ClipboardImage(PathBuf),
    /// `paste` / `paste-plain`.
    Paste {
        /// Ctrl+Shift+V.
        plain: bool,
    },
    /// `tab <n>` (1-based).
    Tab(usize),
    /// `advance <ms>`.
    Advance(Duration),
    /// `sleep <ms>`.
    Sleep(Duration),
    /// `wait-idle`.
    WaitIdle,
    /// `wait-external [ms]`.
    WaitExternal(Duration),
    /// `external-write <path> <text>`.
    ExternalWrite {
        /// The file another program writes.
        path: PathBuf,
        /// Its new content.
        text: String,
    },
    /// `assert-text <file>`.
    AssertText(PathBuf),
    /// `assert-text-is <text>`.
    AssertTextIs(String),
    /// `assert-file <path> <expected-file>`.
    AssertFile {
        /// The file under test.
        path: PathBuf,
        /// The file with the expected bytes.
        expected: PathBuf,
    },
    /// `assert-exists <path>`.
    AssertExists(PathBuf),
    /// `assert-no-file <path>`.
    AssertNoFile(PathBuf),
    /// `assert-magic <path> <kind>`.
    AssertMagic {
        /// The file under test.
        path: PathBuf,
        /// What it must look like.
        magic: Magic,
    },
    /// `assert-dirty <true|false>`.
    AssertDirty(bool),
    /// `assert-tabs <n>`.
    AssertTabs(usize),
    /// `assert-selection <anchor> <head>`.
    AssertSelection {
        /// Where the selection started.
        anchor: usize,
        /// The caret.
        head: usize,
    },
    /// `assert-clipboard <file>`.
    AssertClipboard(PathBuf),
    /// `assert-dialog <kind> <text>`.
    AssertDialog {
        /// The kind of dialog.
        kind: DialogKind,
        /// Text its title or message contains.
        contains: String,
    },
    /// `dump-state <json-path>`.
    DumpState(PathBuf),
    /// `theme light|dark`.
    Theme(ThemeChoice),
    /// `screenshot <png> [width height] [scale]`.
    Screenshot(Shot),
    /// `settings-page <page>`.
    SettingsPage(SettingsSection),
    /// `screenshot-settings <png> [width height] [scale]`.
    ScreenshotSettings(Shot),
    /// `quit`.
    Quit,
}

/// The colors `theme` switches to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeChoice {
    /// The light theme with the default accent.
    Light,
    /// The dark theme with the default accent.
    Dark,
}

impl ThemeChoice {
    fn theme(self) -> Theme {
        match self {
            Self::Light => Theme::light(DEFAULT_ACCENT),
            Self::Dark => Theme::dark(DEFAULT_ACCENT),
        }
    }
}

/// Where and how large a `screenshot` is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shot {
    /// The PNG file to write.
    pub path: PathBuf,
    /// Width in DIPs.
    pub width: u32,
    /// Height in DIPs.
    pub height: u32,
    /// The DPI scale in percent (100 is 96 DPI, 200 is hi-DPI).
    pub scale_percent: u32,
}

impl Shot {
    fn scale(&self) -> f32 {
        self.scale_percent as f32 / 100.0
    }
}

/// Default size of a window screenshot in DIPs.
const WINDOW_SHOT: (u32, u32) = (1440, 900);
/// Default size of a Settings window screenshot in DIPs.
const SETTINGS_SHOT: (u32, u32) = (900, 640);
/// Smallest and largest side of a screenshot in DIPs.
const SHOT_SIDE: std::ops::RangeInclusive<u32> = 64..=8192;
/// Smallest and largest scale of a screenshot.
const SHOT_SCALE: std::ops::RangeInclusive<f32> = 0.5..=4.0;
/// Animation frame times (ms) delivered before a screenshot: the first fixes the start time of
/// every running transition, the later ones finish it.
const SETTLE_FRAMES: [u64; 3] = [0, 1_000, 2_000];

/// A step and the line it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// The 1-based line.
    pub line: usize,
    /// What to do.
    pub op: Op,
}

/// A parsed script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    steps: Vec<Step>,
}

impl Script {
    /// Parses the text of a script file.
    ///
    /// # Errors
    /// [`ScriptError::Syntax`] naming the first invalid line.
    pub fn parse(source: &str) -> Result<Self, ScriptError> {
        let mut steps = Vec::new();
        for (index, text) in source.lines().enumerate() {
            let line = index + 1;
            let ops = parse_line(text).map_err(|message| ScriptError::Syntax { line, message })?;
            steps.extend(ops.into_iter().map(|op| Step { line, op }));
        }
        Ok(Self { steps })
    }

    /// The steps in order.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
}

type Parsed<T> = Result<T, String>;

/// The next word of `rest` (quoted or not) and what follows it.
fn next_word(rest: &str) -> Parsed<Option<(String, &str)>> {
    let rest = rest.trim_start();
    let Some(first) = rest.chars().next() else {
        return Ok(None);
    };
    if first != '"' {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        return Ok(Some((rest[..end].to_owned(), &rest[end..])));
    }
    let mut word = String::new();
    let mut chars = rest.char_indices().skip(1).peekable();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return Ok(Some((word, &rest[index + 1..]))),
            '\\' if chars.next_if(|(_, next)| *next == '"').is_some() => word.push('"'),
            other => word.push(other),
        }
    }
    Err("a quote is not closed".to_owned())
}

/// Every word of `rest`.
fn words(rest: &str) -> Parsed<Vec<String>> {
    let mut all = Vec::new();
    let mut rest = rest;
    while let Some((word, after)) = next_word(rest)? {
        all.push(word);
        rest = after;
    }
    Ok(all)
}

/// Exactly `count` words.
fn exactly(name: &str, rest: &str, count: usize) -> Parsed<Vec<String>> {
    let all = words(rest)?;
    if all.len() == count {
        Ok(all)
    } else {
        Err(format!(
            "{name} takes {count} argument(s), found {}",
            all.len()
        ))
    }
}

fn one(name: &str, rest: &str) -> Parsed<String> {
    Ok(exactly(name, rest, 1)?.remove(0))
}

fn path_arg(name: &str, rest: &str) -> Parsed<PathBuf> {
    one(name, rest).map(PathBuf::from)
}

fn number(name: &str, word: &str) -> Parsed<usize> {
    word.parse()
        .map_err(|_| format!("{name} needs a number, found `{word}`"))
}

fn millis(name: &str, word: &str) -> Parsed<Duration> {
    word.parse()
        .map(Duration::from_millis)
        .map_err(|_| format!("{name} needs a number of milliseconds, found `{word}`"))
}

fn no_args(name: &str, rest: &str) -> Parsed<()> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(format!("{name} takes no arguments"))
    }
}

/// The text of a step: the rest of the line with its escapes resolved.
fn text_arg(name: &str, rest: &str) -> Parsed<String> {
    if rest.is_empty() {
        return Err(format!("{name} needs text"));
    }
    unescape(rest)
}

fn unescape(text: &str) -> Parsed<String> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => return Err(format!("unknown escape `\\{other}`")),
            None => return Err("the text ends with a backslash".to_owned()),
        }
    }
    Ok(out)
}

fn flag(name: &str, rest: &str) -> Parsed<bool> {
    match one(name, rest)?.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("{name} needs true or false, found `{other}`")),
    }
}

fn dialog_kind(word: &str) -> Parsed<DialogKind> {
    DialogKind::from_name(word).ok_or_else(|| format!("unknown dialog kind `{word}`"))
}

fn shot_side(name: &str, word: &str) -> Parsed<u32> {
    word.parse::<u32>()
        .ok()
        .filter(|side| SHOT_SIDE.contains(side))
        .ok_or_else(|| {
            format!(
                "{name} needs a size between {} and {} DIPs, found `{word}`",
                SHOT_SIDE.start(),
                SHOT_SIDE.end()
            )
        })
}

fn shot_scale(name: &str, word: &str) -> Parsed<u32> {
    word.parse::<f32>()
        .ok()
        .filter(|scale| SHOT_SCALE.contains(scale))
        .map(|scale| (scale * 100.0).round() as u32)
        .ok_or_else(|| format!("{name} needs a scale between 0.5 and 4, found `{word}`"))
}

/// `<png> [width height] [scale]`.
fn parse_shot(name: &str, rest: &str, default: (u32, u32)) -> Parsed<Shot> {
    let all = words(rest)?;
    let Some((path, extra)) = all.split_first() else {
        return Err(format!("{name} needs a path"));
    };
    let ((width, height), scale_percent) = match extra {
        [] => (default, 100),
        [scale] => (default, shot_scale(name, scale)?),
        [width, height] => ((shot_side(name, width)?, shot_side(name, height)?), 100),
        [width, height, scale] => (
            (shot_side(name, width)?, shot_side(name, height)?),
            shot_scale(name, scale)?,
        ),
        _ => {
            return Err(format!(
                "{name} takes a path, then a width and height and a scale, each optional"
            ));
        }
    };
    Ok(Shot {
        path: PathBuf::from(path),
        width,
        height,
        scale_percent,
    })
}

fn parse_answer(rest: &str) -> Parsed<Op> {
    let all = words(rest)?;
    let Some((kind, choice)) = all.split_first() else {
        return Err("answer needs a dialog kind and a choice".to_owned());
    };
    let kind = dialog_kind(kind)?;
    let Some(first) = choice.first() else {
        return Err("answer needs a choice".to_owned());
    };
    let answer = if kind == DialogKind::MessageBox {
        if choice.len() != 1 {
            return Err("a message box takes one button".to_owned());
        }
        Answer::Button(match first.as_str() {
            "ok" => MessageBoxResult::Ok,
            "cancel" => MessageBoxResult::Cancel,
            "yes" => MessageBoxResult::Yes,
            "no" => MessageBoxResult::No,
            "save" => MessageBoxResult::Save,
            "dont-save" => MessageBoxResult::DontSave,
            other => return Err(format!("unknown button `{other}`")),
        })
    } else if choice.len() == 1 && first == "cancel" {
        Answer::Cancel
    } else {
        Answer::Paths(choice.iter().map(PathBuf::from).collect())
    };
    Ok(Op::Answer(kind, answer))
}

fn parse_line(line: &str) -> Parsed<Vec<Op>> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return Ok(Vec::new());
    }
    let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let op = match name {
        "open" => Op::Open(path_arg(name, rest)?),
        "new" => {
            no_args(name, rest)?;
            Op::Command(CommandId::FileNew)
        }
        "save" => {
            no_args(name, rest)?;
            Op::Command(CommandId::FileSave)
        }
        "save-as" => Op::SaveAs(path_arg(name, rest)?),
        "command" => Op::Command(
            one(name, rest)?
                .parse()
                .map_err(|error: ssw_core::command::UnknownCommand| error.to_string())?,
        ),
        "type" => Op::Type(text_arg(name, rest)?),
        "key" => Op::Key(
            one(name, rest)?
                .parse()
                .map_err(|error: ssw_core::keymap::ChordParseError| error.to_string())?,
        ),
        "select" => {
            let all = exactly(name, rest, 2)?;
            Op::Select {
                anchor: number(name, &all[0])?,
                head: number(name, &all[1])?,
            }
        }
        "select-text" => Op::SelectText(text_arg(name, rest)?),
        "answer" => parse_answer(rest)?,
        "clipboard-text" => Op::ClipboardText(text_arg(name, rest)?), // docs-check: not a message id
        "clipboard-html" => Op::ClipboardHtml(path_arg(name, rest)?), // docs-check: not a message id
        "clipboard-image" => Op::ClipboardImage(path_arg(name, rest)?), // docs-check: not a message id
        "paste" | "paste-plain" => {
            no_args(name, rest)?;
            Op::Paste {
                plain: name == "paste-plain",
            }
        }
        "paste-text" | "paste-html" | "paste-image" => {
            let prepare = match name {
                "paste-text" => Op::ClipboardText(text_arg(name, rest)?),
                "paste-html" => Op::ClipboardHtml(path_arg(name, rest)?),
                _ => Op::ClipboardImage(path_arg(name, rest)?),
            };
            return Ok(vec![prepare, Op::Paste { plain: false }]);
        }
        "tab" => Op::Tab(number(name, &one(name, rest)?)?),
        "advance" => Op::Advance(millis(name, &one(name, rest)?)?),
        "sleep" => Op::Sleep(millis(name, &one(name, rest)?)?),
        "wait-idle" => {
            no_args(name, rest)?;
            Op::WaitIdle
        }
        "wait-external" => match words(rest)?.as_slice() {
            [] => Op::WaitExternal(DEFAULT_EXTERNAL_WAIT),
            [limit] => Op::WaitExternal(millis(name, limit)?),
            _ => return Err(format!("{name} takes at most one argument")),
        },
        "external-write" => {
            let (path, after) =
                next_word(rest)?.ok_or_else(|| format!("{name} needs a path and a text"))?;
            let text = after.strip_prefix(char::is_whitespace).unwrap_or(after);
            Op::ExternalWrite {
                path: PathBuf::from(path),
                text: unescape(text)?,
            }
        }
        "assert-text" => Op::AssertText(path_arg(name, rest)?),
        "assert-text-is" => Op::AssertTextIs(unescape(rest)?),
        "assert-file" => {
            let all = exactly(name, rest, 2)?;
            Op::AssertFile {
                path: PathBuf::from(&all[0]),
                expected: PathBuf::from(&all[1]),
            }
        }
        "assert-exists" => Op::AssertExists(path_arg(name, rest)?),
        "assert-no-file" => Op::AssertNoFile(path_arg(name, rest)?),
        "assert-magic" => {
            let all = exactly(name, rest, 2)?;
            Op::AssertMagic {
                path: PathBuf::from(&all[0]),
                magic: Magic::from_name(&all[1])
                    .ok_or_else(|| format!("unknown file kind `{}`", all[1]))?,
            }
        }
        "assert-dirty" => Op::AssertDirty(flag(name, rest)?),
        "assert-tabs" => Op::AssertTabs(number(name, &one(name, rest)?)?),
        "assert-selection" => {
            let all = exactly(name, rest, 2)?;
            Op::AssertSelection {
                anchor: number(name, &all[0])?,
                head: number(name, &all[1])?,
            }
        }
        "assert-clipboard" => Op::AssertClipboard(path_arg(name, rest)?),
        "assert-dialog" => {
            let (kind, after) =
                next_word(rest)?.ok_or_else(|| format!("{name} needs a dialog kind and a text"))?;
            let after = after.strip_prefix(char::is_whitespace).unwrap_or(after);
            Op::AssertDialog {
                kind: dialog_kind(&kind)?,
                contains: text_arg(name, after)?,
            }
        }
        "dump-state" => Op::DumpState(path_arg(name, rest)?),
        "theme" => Op::Theme(match one(name, rest)?.as_str() {
            "light" => ThemeChoice::Light,
            "dark" => ThemeChoice::Dark,
            other => return Err(format!("unknown theme `{other}` (light or dark)")),
        }),
        "screenshot" => Op::Screenshot(parse_shot(name, rest, WINDOW_SHOT)?),
        "screenshot-settings" => Op::ScreenshotSettings(parse_shot(name, rest, SETTINGS_SHOT)?),
        "settings-page" => Op::SettingsPage(settings_page(name, rest)?), // docs-check: not a message id
        "quit" => {
            no_args(name, rest)?;
            Op::Quit
        }
        other => return Err(format!("unknown step `{other}`")),
    };
    Ok(vec![op])
}

/// The Settings page a `settings-page` step names.
fn settings_page(name: &str, rest: &str) -> Result<SettingsSection, String> {
    let page = one(name, rest)?;
    SettingsSection::ALL
        .into_iter()
        .find(|section| section.key() == page)
        .ok_or_else(|| format!("unknown settings page `{page}`"))
}

/// The keyboard layout of the harness has no AltGr: Ctrl+Alt+key is always a shortcut.
struct NoAltGr;

impl CharProbe for NoAltGr {
    fn produces_char(&self, _vk: u16, _scan: u16, _shift: bool, _caps: bool) -> bool {
        false
    }
}

/// The key event of a chord.
fn key_event(chord: KeyChord) -> KeyEvent {
    let mut mods = Modifiers::NONE;
    for (held, modifier) in [
        (chord.ctrl, Modifiers::CTRL),
        (chord.shift, Modifiers::SHIFT),
        (chord.alt, Modifiers::ALT),
        (chord.win, Modifiers::WIN),
    ] {
        if held {
            mods |= modifier;
        }
    }
    KeyEvent::press(chord.vk, mods)
}

/// The result of one step: the failure message.
type StepResult = Result<(), String>;

fn read_text(path: &Path) -> Parsed<String> {
    std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn read_bytes(path: &Path) -> Parsed<Vec<u8>> {
    std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn same_text(what: &str, expected: &str, actual: &str) -> StepResult {
    if expected == actual {
        Ok(())
    } else {
        Err(format!(
            "{what} differs\n  expected: {expected:?}\n  actual:   {actual:?}"
        ))
    }
}

/// Runs a script against the controller.
pub struct ScriptRunner {
    controller: Rc<Controller>,
    services: ScriptedServices,
    clipboard: Rc<RefCell<MemoryClipboard>>,
    clock: Rc<FakeClock>,
    engine: Box<dyn TextLayoutEngine>,
    pending: ClipboardContent,
    wait_limit: Duration,
    quit: bool,
    external: Vec<String>,
    autosave_errors: Vec<String>,
    unhandled: Vec<String>,
    /// The windows (ids) whose editors got the block renderers of [`Visuals::editor_setup`].
    equipped: Vec<u64>,
    /// The offscreen renderer, once `theme` or a screenshot step started it. Declared last: it
    /// drops after the controller, whose editors hold COM objects of it.
    visuals: Option<Rc<Visuals>>,
}

impl fmt::Debug for ScriptRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptRunner")
            .field("controller", &self.controller)
            .field("quit", &self.quit)
            .finish_non_exhaustive()
    }
}

impl ScriptRunner {
    /// A runner over `core`: the windows and tabs of its startup plan exist, the OS services
    /// are the scripted ones.
    #[must_use]
    pub fn start(core: AppCore) -> Self {
        let services = ScriptedServices::new();
        let clipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let clock = Rc::new(FakeClock::default());
        let plan = core.plan.clone();
        let shared = Rc::clone(&clipboard) as SharedClipboard;
        let time = Rc::clone(&clock) as Rc<dyn Clock>;
        let controller = Controller::new(core, services.dialogs(), services.shell(), shared, time);
        controller.open_plan(&plan);
        Self {
            controller,
            services,
            clipboard,
            clock,
            engine: Box::new(MonoTextEngine::default()),
            pending: ClipboardContent::default(),
            wait_limit: DEFAULT_WAIT_LIMIT,
            quit: false,
            external: Vec::new(),
            autosave_errors: Vec::new(),
            unhandled: Vec::new(),
            equipped: Vec::new(),
            visuals: None,
        }
    }

    /// The controller under test.
    #[must_use]
    pub fn controller(&self) -> &Rc<Controller> {
        &self.controller
    }

    /// How long `wait-idle` waits for the workers before it fails.
    pub fn set_wait_limit(&mut self, limit: Duration) {
        self.wait_limit = limit;
    }

    /// Runs every step; stops at the first failure or at `quit`. A dialog nobody answered, or
    /// an answer no dialog asked for, fails the script too.
    ///
    /// # Errors
    /// [`ScriptError::Failed`] for the first failing step.
    pub fn run(&mut self, script: &Script) -> Result<(), ScriptError> {
        self.settle().map_err(|message| ScriptError::Failed {
            line: 0,
            message: format!("at startup: {message}"),
        })?;
        for step in script.steps() {
            let outcome = self.step(&step.op);
            let outcome = outcome.and_then(|()| self.settle());
            outcome.map_err(|message| ScriptError::Failed {
                line: step.line,
                message,
            })?;
            if self.quit {
                break;
            }
        }
        match self.services.unused_answers().first() {
            Some((kind, answer)) => Err(ScriptError::Failed {
                line: script.steps().last().map_or(0, |step| step.line),
                message: format!("no {} dialog asked for the answer {answer:?}", kind.name()),
            }),
            None => Ok(()),
        }
    }

    /// The clean exit: autosave flush, session, recovery cleanup.
    pub fn finish(&mut self) {
        let session = self.controller.session();
        self.controller.core().borrow_mut().shutdown(&session);
    }

    // ----- the message loop's share --------------------------------------------------------

    /// Carries out what the windows asked for, runs one housekeeping turn, and checks that no
    /// unprepared dialog appeared.
    fn settle(&mut self) -> StepResult {
        self.drain_requests();
        self.pump();
        self.drain_requests();
        match self.services.take_unexpected() {
            Some(dialog) => Err(format!("unexpected dialog: {dialog}")),
            None => Ok(()),
        }
    }

    fn drain_requests(&mut self) {
        while let Some(request) = self.controller.take_request() {
            match request {
                Request::Command(window, command) => {
                    if matches!(
                        self.controller.run_command(window, command),
                        Dispatch::Unhandled
                    ) {
                        self.unhandled.push(format!("command {command}"));
                    }
                }
                Request::Action(window, action) => self.controller.handle_action(window, action),
                Request::Quit => self.quit = true,
                Request::OpenSettings => {
                    self.controller.create_settings(SettingsSetup {
                        probe: Box::new(NoAltGr),
                        key_names: Box::new(WindowsKeyNames),
                        motion: Motion::reduced(),
                        size: WINDOW_SIZE,
                        scale: 1.0,
                    });
                }
                Request::Settings(event) => self.controller.settings_event(event),
                Request::CloseSettings => self.controller.close_settings(),
                // There are no native windows to create or destroy, and no taskbar button.
                Request::NewWindow(_) | Request::CloseWindow(_) | Request::TaskbarProgress(..) => {}
                other => self.unhandled.push(format!("{other:?}")),
            }
        }
    }

    fn pump(&mut self) {
        let report = self.controller.core().borrow_mut().pump();
        self.controller.pump_files();
        self.note(&report);
        self.controller.refresh_all();
    }

    fn note(&mut self, report: &PumpReport) {
        for outcome in &report.external {
            let path = outcome.entry.path().unwrap_or_default();
            self.external
                .push(format!("{}: {:?}", path.display(), outcome.action));
        }
        for (_, error) in &report.tick.failed {
            self.autosave_errors.push(error.to_string());
        }
    }

    /// Pumps until `done` holds or `limit` passes.
    fn wait_until(
        &mut self,
        limit: Duration,
        what: &str,
        done: impl Fn(&Self) -> bool,
    ) -> StepResult {
        let deadline = Instant::now() + limit;
        loop {
            self.pump();
            if done(self) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{what} did not happen within {} ms",
                    limit.as_millis()
                ));
            }
            std::thread::sleep(WAIT_POLL);
        }
    }

    // ----- steps ---------------------------------------------------------------------------

    fn step(&mut self, op: &Op) -> StepResult {
        match op {
            Op::Open(path) => self.open(path),
            Op::Command(command) => self.command(*command),
            Op::Type(text) => self.type_text(text),
            Op::Key(chord) => self.press(key_event(*chord)),
            Op::Select { anchor, head } => self.select(*anchor, *head),
            Op::SelectText(needle) => self.select_text(needle),
            Op::SaveAs(path) => self.save_as(path),
            Op::Answer(kind, answer) => {
                self.services.answer(*kind, answer.clone());
                Ok(())
            }
            Op::ClipboardText(text) => {
                self.pending.text = Some(text.clone());
                Ok(())
            }
            Op::ClipboardHtml(path) => {
                self.pending.html = Some(read_text(path)?);
                Ok(())
            }
            Op::ClipboardImage(path) => {
                self.pending.image = Some(read_bytes(path)?);
                Ok(())
            }
            Op::Paste { plain } => self.paste(*plain),
            Op::Tab(number) => self.tab(*number),
            Op::Advance(by) => {
                self.clock.advance(*by);
                Ok(())
            }
            Op::Sleep(by) => {
                std::thread::sleep(*by);
                Ok(())
            }
            Op::WaitIdle => {
                let limit = self.wait_limit;
                self.wait_until(limit, "the workers becoming idle", |runner| {
                    runner.controller.core().borrow().workers.pending() == 0
                })
            }
            Op::WaitExternal(limit) => {
                let seen = self.external.len();
                self.wait_until(*limit, "an external change", move |runner| {
                    runner.external.len() > seen
                })
            }
            Op::ExternalWrite { path, text } => std::fs::write(path, text)
                .map_err(|error| format!("cannot write {}: {error}", path.display())),
            Op::AssertText(path) => {
                let expected = read_text(path)?;
                same_text("the document text", &expected, &self.active_text()?)
            }
            Op::AssertTextIs(expected) => {
                same_text("the document text", expected, &self.active_text()?)
            }
            Op::AssertFile { path, expected } => assert_file(path, expected),
            Op::AssertExists(path) => {
                if path.exists() {
                    Ok(())
                } else {
                    Err(format!("{} does not exist", path.display()))
                }
            }
            Op::AssertNoFile(path) => {
                if path.exists() {
                    Err(format!("{} exists", path.display()))
                } else {
                    Ok(())
                }
            }
            Op::AssertMagic { path, magic } => {
                if magic.matches(&read_bytes(path)?) {
                    Ok(())
                } else {
                    Err(format!("{} is not a {magic:?} file", path.display()))
                }
            }
            Op::AssertDirty(expected) => {
                let dirty = self.with_tab(|tab| tab.entry.is_dirty())?;
                if dirty == *expected {
                    Ok(())
                } else {
                    Err(format!(
                        "the document is {}",
                        if dirty { "dirty" } else { "clean" }
                    ))
                }
            }
            Op::AssertTabs(expected) => {
                let count = self.window()?.view.borrow().tabs().len();
                if count == *expected {
                    Ok(())
                } else {
                    Err(format!(
                        "the window has {count} tab(s), expected {expected}"
                    ))
                }
            }
            Op::AssertSelection { anchor, head } => {
                let selection =
                    self.with_tab(|tab| tab.entry.document().borrow().primary_selection())?;
                if (selection.anchor, selection.head) == (*anchor, *head) {
                    Ok(())
                } else {
                    Err(format!(
                        "the selection is {}..{}, expected {anchor}..{head}",
                        selection.anchor, selection.head
                    ))
                }
            }
            Op::AssertClipboard(path) => {
                let expected = read_text(path)?;
                let actual = self
                    .clipboard
                    .borrow()
                    .text()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                same_text("the clipboard text", &expected, &actual)
            }
            Op::AssertDialog { kind, contains } => {
                let shown = self.services.dialog_log().into_iter().any(|record| {
                    record.kind == *kind
                        && (record.message.contains(contains.as_str())
                            || record.title.contains(contains.as_str()))
                });
                if shown {
                    Ok(())
                } else {
                    Err(format!("no {} dialog mentioned {contains:?}", kind.name()))
                }
            }
            Op::DumpState(path) => {
                let state = self.state();
                std::fs::write(path, format!("{state:#}"))
                    .map_err(|error| format!("cannot write {}: {error}", path.display()))
            }
            Op::Theme(choice) => self.set_theme(*choice),
            Op::Screenshot(shot) => self.screenshot(shot),
            Op::SettingsPage(section) => self.settings_page(*section),
            Op::ScreenshotSettings(shot) => self.screenshot_settings(shot),
            Op::Quit => {
                self.quit = true;
                Ok(())
            }
        }
    }

    fn window(&self) -> Result<Rc<WindowCell>, String> {
        self.controller
            .active_window()
            .ok_or_else(|| "the application has no window".to_owned())
    }

    /// Runs `read` on the active tab.
    fn with_tab<R>(&self, read: impl FnOnce(&TabView) -> R) -> Result<R, String> {
        let window = self.window()?;
        let view = window.view.borrow();
        view.active_tab()
            .map(read)
            .ok_or_else(|| "the window has no document".to_owned())
    }

    fn active_text(&self) -> Result<String, String> {
        self.with_tab(|tab| tab.entry.document().borrow().text())
    }

    fn open(&mut self, path: &Path) -> StepResult {
        let window = self.window()?;
        self.controller
            .open_file(&window, path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))
    }

    fn command(&mut self, command: CommandId) -> StepResult {
        let window = self.window()?;
        match self.controller.run_command(window.id, command) {
            Dispatch::Handled => Ok(()),
            Dispatch::Unhandled => Err(format!("{command} was not handled")),
            Dispatch::Failed(error) => Err(format!("{command} failed: {error}")),
        }
    }

    /// Delivers an event to the window content and carries out what it asks for.
    fn deliver(&mut self, event: &Event) -> StepResult {
        let window = self.window()?;
        self.deliver_to(&window, event);
        Ok(())
    }

    /// [`ScriptRunner::deliver`] to a window that is known to exist.
    fn deliver_to(&self, window: &WindowCell, event: &Event) {
        let actions = window.view.borrow_mut().event(event, &*self.engine);
        for action in actions {
            self.controller.handle_action(window.id, action);
        }
    }

    /// A key press: a shortcut runs its command, anything else goes to the focused widget.
    fn press(&mut self, event: KeyEvent) -> StepResult {
        match self.controller.route_key(&event, &NoAltGr, false) {
            ShortcutOutcome::Command(command) => self.command(command),
            ShortcutOutcome::Unhandled => self.deliver(&Event::Key(event)),
            ShortcutOutcome::Text | ShortcutOutcome::OpenMenu | ShortcutOutcome::Mnemonic(_) => {
                Err("the key opens a menu, which needs a native window".to_owned())
            }
        }
    }

    fn type_text(&mut self, text: &str) -> StepResult {
        for c in text.chars() {
            match c {
                '\n' => self.press(KeyEvent::press(vk::RETURN, Modifiers::NONE))?,
                '\t' => self.press(KeyEvent::press(vk::TAB, Modifiers::NONE))?,
                other => self.deliver(&Event::Text(TextInput(other.to_string())))?,
            }
        }
        Ok(())
    }

    /// Sets the selection of the active editor to what `choose` makes of the document's text.
    /// One look at the window and its tab: the text and the editor belong to the same tab.
    fn select_in_text(
        &mut self,
        choose: impl FnOnce(&str) -> Result<Selection, String>,
    ) -> StepResult {
        let window = self.window()?;
        let mut view = window.view.borrow_mut();
        let tab = view
            .active_tab_mut()
            .ok_or_else(|| "the window has no document".to_owned())?;
        let selection = choose(&tab.entry.document().borrow().text())?;
        tab.editor.set_selection(selection);
        Ok(())
    }

    fn select(&mut self, anchor: usize, head: usize) -> StepResult {
        self.select_in_text(|text| {
            for offset in [anchor, head] {
                if !text.is_char_boundary(offset) {
                    return Err(format!(
                        "{offset} is not a character boundary of the {}-byte text",
                        text.len()
                    ));
                }
            }
            Ok(Selection::new(anchor, head))
        })
    }

    fn select_text(&mut self, needle: &str) -> StepResult {
        self.select_in_text(|text| {
            let start = text
                .find(needle)
                .ok_or_else(|| format!("the text does not contain {needle:?}"))?;
            Ok(Selection::new(start, start + needle.len()))
        })
    }

    fn save_as(&mut self, path: &Path) -> StepResult {
        self.services.answer(
            DialogKind::SaveFile,
            Answer::Paths(vec![path.to_path_buf()]),
        );
        self.command(CommandId::FileSaveAs)?;
        let waiting = self
            .services
            .unused_answers()
            .iter()
            .any(|(kind, _)| *kind == DialogKind::SaveFile);
        if waiting {
            Err("save as showed no save dialog".to_owned())
        } else {
            Ok(())
        }
    }

    fn paste(&mut self, plain: bool) -> StepResult {
        let content = std::mem::take(&mut self.pending);
        // The in-memory clipboard never fails.
        let _ = self.clipboard.borrow_mut().set(content);
        self.command(if plain {
            CommandId::EditPastePlain
        } else {
            CommandId::EditPaste
        })
    }

    fn tab(&mut self, number: usize) -> StepResult {
        let window = self.window()?;
        let key = {
            let view = window.view.borrow();
            number
                .checked_sub(1)
                .and_then(|index| view.tabs().get(index))
                .map(|tab| tab.key)
        }
        .ok_or_else(|| format!("the window has no tab {number}"))?;
        self.controller.handle_action(
            window.id,
            ShellAction::Title(TitleEvent::Tabs(TabsEvent::Activated(key))),
        );
        Ok(())
    }

    // ----- screenshots ---------------------------------------------------------------------

    /// Starts the offscreen renderer (once): the real text engine from now on, and the block
    /// renderers of the window in every editor that does not have them yet.
    fn begin_visuals(&mut self) -> Result<Rc<Visuals>, String> {
        let visuals = match &self.visuals {
            Some(visuals) => Rc::clone(visuals),
            None => {
                let visuals = Rc::new(Visuals::new()?);
                self.engine = Box::new(visuals.text().clone());
                self.visuals = Some(Rc::clone(&visuals));
                visuals
            }
        };
        let setup = visuals.editor_setup(self.controller.i18n());
        for cell in self.controller.windows() {
            if !self.equipped.contains(&cell.id.0) {
                cell.view.borrow_mut().set_editor_setup(Rc::clone(&setup));
                self.equipped.push(cell.id.0);
            }
        }
        Ok(visuals)
    }

    fn set_theme(&mut self, choice: ThemeChoice) -> StepResult {
        self.begin_visuals()?;
        let theme = choice.theme();
        self.controller.set_theme(theme);
        if let Some(cell) = self.controller.settings_cell() {
            cell.view.borrow_mut().set_theme(theme);
        }
        Ok(())
    }

    /// Delivers the animation frames a window would have had: every transition (hover, toggle,
    /// expansion) ends at its target before the picture is taken.
    /// The frames go to `window`; what the Settings window reports meanwhile is carried out.
    fn finish_animations(&self, window: &WindowCell) {
        for millis in SETTLE_FRAMES {
            let frame = Event::AnimationFrame(Duration::from_millis(millis));
            self.deliver_to(window, &frame);
            if let Some(cell) = self.controller.settings_cell() {
                let events = cell.view.borrow_mut().event(&frame, &*self.engine);
                for event in events {
                    self.controller.settings_event(event);
                }
            }
        }
    }

    fn screenshot(&mut self, shot: &Shot) -> StepResult {
        let visuals = self.begin_visuals()?;
        let geometry = Geometry::new(shot.width, shot.height, shot.scale())?;
        let window = self.window()?;
        self.finish_animations(&window);
        let theme = self.controller.theme();
        let mut view = window.view.borrow_mut();
        visuals.set_base(
            view.active_tab()
                .and_then(|tab| tab.entry.path())
                .and_then(|path| path.parent().map(Path::to_path_buf)),
        );
        // The base folder and the text engine changed what the objects and lines look like.
        for tab in view.tabs_mut() {
            tab.editor.invalidate_objects();
        }
        view.resize(geometry.size(), geometry.scale());
        let width = geometry.size().width;
        let chrome = view.layout().mode().shows_chrome();
        let text = visuals.text().clone();
        write_shot(
            &visuals,
            shot,
            geometry,
            theme.tokens.window_bg,
            &mut |canvas| {
                view.paint(canvas, &text, false);
                view.requests_mut().take_invalidation();
                if chrome {
                    paint_caption_buttons(canvas, width, ChromeState::default(), &theme.tokens);
                }
            },
        )
    }

    fn open_settings_cell(&self) -> Result<Rc<SettingsCell>, String> {
        self.controller.settings_cell().ok_or_else(|| {
            "the Settings window is not open (command settings.open opens it)".to_owned()
        })
    }

    fn settings_page(&mut self, section: SettingsSection) -> StepResult {
        let cell = self.open_settings_cell()?;
        let mut view = cell.view.borrow_mut();
        view.window_mut().select_section(section);
        view.requests_mut().invalidate_all();
        Ok(())
    }

    fn screenshot_settings(&mut self, shot: &Shot) -> StepResult {
        let visuals = self.begin_visuals()?;
        let cell = self.open_settings_cell()?;
        let geometry = Geometry::new(shot.width, shot.height, shot.scale())?;
        let window = self.window()?;
        self.finish_animations(&window);
        let theme = self.controller.theme();
        let mut view = cell.view.borrow_mut();
        view.resize(geometry.size(), geometry.scale());
        let width = geometry.size().width;
        let text = visuals.text().clone();
        write_shot(
            &visuals,
            shot,
            geometry,
            theme.tokens.window_bg,
            &mut |canvas| {
                view.paint(canvas, &text, false);
                view.requests_mut().take_invalidation();
                paint_caption_buttons(canvas, width, ChromeState::default(), &theme.tokens);
            },
        )
    }

    // ----- state dump ----------------------------------------------------------------------

    fn state(&self) -> Value {
        let controller = &self.controller;
        let active = controller.active_window().map(|cell| cell.id);
        let windows: Vec<Value> = controller
            .windows()
            .iter()
            .map(|cell| {
                let mut view = cell.view.borrow_mut();
                let banners: Vec<String> = view
                    .banners_mut()
                    .kinds()
                    .iter()
                    .map(|kind| format!("{kind:?}"))
                    .collect();
                let active_key = view.active_tab().map(|tab| tab.key);
                let tabs: Vec<Value> = view
                    .tabs()
                    .iter()
                    .map(|tab| tab_state(controller, tab, active_key))
                    .collect();
                json!({
                    "id": cell.id.0,
                    "active": active == Some(cell.id),
                    "mode": format!("{:?}", view.layout().mode()),
                    "sidebar_visible": view.layout().sidebar_visible(),
                    "inspector_visible": view.layout().inspector_visible(),
                    "status_bar_visible": view.layout().status_bar_visible(),
                    "workspace_shown": cell.tree.borrow().is_some(),
                    "banners": banners,
                    "tabs": tabs,
                })
            })
            .collect();
        let dialogs: Vec<Value> = self
            .services
            .dialog_log()
            .iter()
            .map(|record| {
                json!({
                    "kind": record.kind.name(),
                    "title": record.title,
                    "message": record.message,
                    "answered": record.answered,
                })
            })
            .collect();
        let shell: Vec<String> = self
            .services
            .shell_log()
            .iter()
            .map(|record| format!("{record:?}"))
            .collect();
        let settings = controller.settings();
        let core = controller.core().borrow();
        let clipboard = self.clipboard.borrow();
        json!({
            "windows": windows,
            "settings_path": core.paths.settings_file().to_string_lossy(),
            "data_dir": core.paths.local_dir().to_string_lossy(),
            "zoom_percent": settings.appearance.zoom_percent,
            "autosave": settings.general.autosave,
            "dialogs": dialogs,
            "shell": shell,
            "clipboard": {
                "text": clipboard.text().ok().flatten(),
                "html": clipboard.html().ok().flatten(),
            },
            "external_changes": self.external,
            "autosave_errors": self.autosave_errors,
            "unhandled_requests": self.unhandled,
        })
    }
}

/// Renders `paint` into the PNG of `shot` (`geometry` pixels cleared to `background`).
fn write_shot(
    visuals: &Visuals,
    shot: &Shot,
    geometry: Geometry,
    background: Color,
    paint: &mut dyn FnMut(&mut dyn Canvas),
) -> StepResult {
    let png = visuals.render(geometry, background, paint)?;
    write_png(&shot.path, &png)
}

/// Writes `bytes` to `path`, creating the folders above it.
fn write_png(path: &Path, bytes: &[u8]) -> StepResult {
    let failed = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    std::fs::write(path, bytes).map_err(failed)
}

fn assert_file(path: &Path, expected: &Path) -> StepResult {
    let actual = read_bytes(path)?;
    let wanted = read_bytes(expected)?;
    if actual == wanted {
        return Ok(());
    }
    let first = actual
        .iter()
        .zip(&wanted)
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| actual.len().min(wanted.len()));
    Err(format!(
        "{} differs from {}: {} bytes against {} bytes, first difference at byte {first}",
        path.display(),
        expected.display(),
        actual.len(),
        wanted.len()
    ))
}

fn tab_state(controller: &Controller, tab: &TabView, active_key: Option<u64>) -> Value {
    let entry = &tab.entry;
    let format = entry.format();
    let document = entry.document().borrow();
    let selection = document.primary_selection();
    json!({
        "key": tab.key,
        "active": active_key == Some(tab.key),
        "title": entry.title(controller.i18n()),
        "path": entry.path().map(|path| path.to_string_lossy().into_owned()),
        "untitled": entry.is_untitled(),
        "dirty": entry.is_dirty(),
        "read_only": entry.is_read_only(),
        "external": format!("{:?}", entry.external()),
        "mode": format!("{:?}", entry.mode()),
        "encoding": format.encoding.name(),
        "bom": format.bom,
        "line_ending": format!("{:?}", format.line_ending),
        "workspace": tab.workspace.as_ref().map(|root| root.to_string_lossy().into_owned()),
        "text": document.text(),
        "version": document.version(),
        "selection": { "anchor": selection.anchor, "head": selection.head },
        "losses": entry.losses().len(),
        "zoom_percent": tab.editor.zoom_percent(),
    })
}

/// Runs `script` against `core` and ends the application like a clean exit.
///
/// # Errors
/// [`ScriptError::Failed`] for the first failing step.
pub fn run_script(core: AppCore, script: &Script) -> Result<(), ScriptError> {
    let mut runner = ScriptRunner::start(core);
    let result = runner.run(script);
    runner.finish();
    result
}

/// The shell of `--test-script`: no windows, the script drives the controller.
#[derive(Debug, Default)]
pub struct ScriptHost;

impl ShellHost for ScriptHost {
    fn run(&mut self, core: AppCore, args: &Args) -> i32 {
        // Bound first, so it is dropped after every COM object of the screenshot steps. A thread
        // that cannot have COM only fails the scripts that take screenshots.
        let _com = ssw_win::com::ComGuard::apartment_threaded().ok();
        let Some(path) = args.test.script.as_deref() else {
            eprintln!("SS Writer: --test-script names no script file");
            return EXIT_SCRIPT_UNREADABLE;
        };
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                eprintln!(
                    "SS Writer: cannot read the script {}: {error}",
                    path.display()
                );
                return EXIT_SCRIPT_UNREADABLE;
            }
        };
        let script = match Script::parse(&source) {
            Ok(script) => script,
            Err(error) => {
                eprintln!("SS Writer: invalid script {}: {error}", path.display());
                return EXIT_SCRIPT_INVALID;
            }
        };
        match run_script(core, &script) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("SS Writer: script {} failed: {error}", path.display());
                EXIT_SCRIPT_FAILED
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ssw_io::paths::Environment;
    use ssw_win::gfx::Driver;
    use ssw_win::window::OsVersion;

    use super::*;
    use crate::startup::ProcessEnv;

    /// Hands the core of a real startup to the test.
    struct TakeCore(Option<AppCore>);

    impl ShellHost for TakeCore {
        fn run(&mut self, core: AppCore, _args: &Args) -> i32 {
            self.0 = Some(core);
            0
        }
    }

    fn core(root: &Path) -> AppCore {
        let process = ProcessEnv {
            os: OsVersion {
                major: 10,
                build: 22621,
            },
            env: Environment {
                local_app_data: Some(root.join("local")),
                app_data: Some(root.join("roaming")),
                exe_dir: root.join("exe"),
            },
            os_language: "en-US".to_owned(),
            driver: Driver::Warp,
            install_crash_handler: false,
        };
        let mut dialogs = ScriptedServices::new().dialogs();
        let mut host = TakeCore(None);
        let code = crate::app::run(&Args::default(), process, &mut *dialogs, None, &mut host);
        assert_eq!(code, 0, "the startup ran the shell");
        host.0.take().unwrap()
    }

    fn settings(root: &Path, json: &str) {
        let dir = root.join("roaming").join("SS Writer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), json).unwrap();
    }

    fn run(root: &Path, script: &str) -> Result<(), ScriptError> {
        run_script(core(root), &Script::parse(script).unwrap())
    }

    /// The line and the message of an error of either kind.
    fn located(error: ScriptError) -> (usize, String) {
        let (ScriptError::Failed { line, message } | ScriptError::Syntax { line, message }) = error;
        (line, message)
    }

    fn failure(result: Result<(), ScriptError>) -> (usize, String) {
        // The scripts of these tests parse (`run` unwraps that), so the error is a failed step.
        located(result.unwrap_err())
    }

    fn quoted(path: &Path) -> String {
        format!("\"{}\"", path.display())
    }

    /// A runner that ran `script` in a fresh profile below `root`.
    fn ran(root: &Path, script: &str) -> ScriptRunner {
        let mut runner = ScriptRunner::start(core(root));
        runner.run(&Script::parse(script).unwrap()).unwrap();
        runner
    }

    /// The text of the active document of the first window.
    fn document_text(runner: &ScriptRunner) -> String {
        let window = runner.controller().active_window().unwrap();
        let view = window.view.borrow();
        view.active_tab().unwrap().entry.document().borrow().text()
    }

    /// How the active editor of the first window presents its document.
    fn presented_as(runner: &ScriptRunner) -> ssw_ui::editor::DocumentKind {
        let window = runner.controller().active_window().unwrap();
        let view = window.view.borrow();
        view.active_tab().unwrap().editor.config().document_kind
    }

    #[test]
    fn typing_after_the_replace_command_goes_to_the_find_and_replace_boxes() {
        // The file is opened first: the tab's pending return of the focus to the editor must
        // not take the keyboard from the bar that opens after it.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "# Title\n").unwrap();
        let script = format!(
            "open {}\ncommand search.replace\ntype launch\nkey tab\ntype release\n",
            quoted(&file)
        );
        let mut runner = ran(tmp.path(), &script);
        {
            let window = runner.controller().active_window().unwrap();
            let view = window.view.borrow();
            let find = view.overlays().find();
            assert!(find.is_replace_open());
            assert_eq!(find.find_box().text(), "launch");
            assert_eq!(find.replace_box().text(), "release");
        }
        assert_eq!(document_text(&runner), "# Title\n");
        runner.finish();
    }

    #[test]
    fn typing_after_quick_open_and_the_palette_goes_to_their_boxes() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "# Title\n").unwrap();
        let script = format!(
            "open {}\ncommand search.quick_open\nwait-idle\ntype atl\n",
            quoted(&file)
        );
        let mut runner = ran(tmp.path(), &script);
        let typed = |runner: &ScriptRunner| {
            let window = runner.controller().active_window().unwrap();
            let view = window.view.borrow();
            view.overlays().quick_open().input().text().to_owned()
        };
        assert_eq!(typed(&runner), "atl");
        assert_eq!(document_text(&runner), "# Title\n");
        let palette = "key escape\ncommand search.command_palette\nwait-idle\ntype export\n";
        runner.run(&Script::parse(palette).unwrap()).unwrap();
        assert_eq!(typed(&runner), ">export");
        assert_eq!(document_text(&runner), "# Title\n");
        runner.finish();
    }

    #[test]
    fn a_document_is_presented_the_way_its_file_type_wants() {
        let tmp = tempfile::tempdir().unwrap();
        let (diagram, plain, note) = (
            tmp.path().join("d.mmd"),
            tmp.path().join("t.txt"),
            tmp.path().join("n.md"),
        );
        std::fs::write(&diagram, "flowchart LR\n  A --> B\n").unwrap();
        std::fs::write(&plain, "# not a heading\n").unwrap();
        std::fs::write(&note, "# heading\n").unwrap();
        let mut runner = ran(tmp.path(), &format!("open {}\n", quoted(&diagram)));
        assert_eq!(presented_as(&runner), ssw_ui::editor::DocumentKind::Mermaid);
        for (file, kind) in [
            (&plain, ssw_ui::editor::DocumentKind::PlainText),
            (&note, ssw_ui::editor::DocumentKind::Markdown),
        ] {
            let script = Script::parse(&format!("open {}\n", quoted(file))).unwrap();
            runner.run(&script).unwrap();
            assert_eq!(presented_as(&runner), kind);
        }
        runner.finish();
    }

    #[test]
    fn saving_an_untitled_document_under_a_plain_name_changes_how_it_is_presented() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.txt");
        let script = format!("new\ntype hello\nsave-as {}\n", quoted(&out));
        let mut runner = ran(tmp.path(), &script);
        assert_eq!(
            presented_as(&runner),
            ssw_ui::editor::DocumentKind::PlainText
        );
        runner.finish();
    }

    #[test]
    fn every_step_parses_with_its_arguments() {
        let script = Script::parse(
            r#"
# a comment

open "C:\Notes\a b.md"
new
save
save-as out.md
command format.bold
type Hello\nWorld\t!\\
key ctrl+shift+z
select 3 7
select-text two words
answer message-box dont-save
answer open-file a.md "b c.md"
answer save-file cancel
clipboard-text a\tb
clipboard-html page.html
clipboard-image pic.png
paste
paste-plain
paste-text plain
paste-html page.html
paste-image pic.png
tab 2
advance 1500
sleep 20
wait-idle
wait-external
wait-external 250
external-write "x y.md" new\ntext
assert-text expected.md
assert-text-is two\nlines
assert-file out.md expected.md
assert-exists a.md
assert-no-file b.md
assert-magic out.docx docx
assert-dirty false
assert-tabs 2
assert-selection 1 2
assert-clipboard clip.txt
assert-dialog message-box Unsaved changes
dump-state state.json
theme dark
theme light
screenshot shot.png
screenshot "my shot.png" 2
screenshot a.png 800 600
screenshot a.png 800 600 1.5
settings-page appearance
screenshot-settings s.png
screenshot-settings s.png 450 320 2.0
quit
"#,
        )
        .unwrap();
        assert_eq!(script.steps()[0].line, 4);
        let ops: Vec<Op> = script.steps().iter().map(|step| step.op.clone()).collect();
        let p = PathBuf::from;
        let shot = |path: &str, width: u32, height: u32, scale_percent: u32| Shot {
            path: PathBuf::from(path),
            width,
            height,
            scale_percent,
        };
        let expected = vec![
            Op::Open(p("C:\\Notes\\a b.md")),
            Op::Command(CommandId::FileNew),
            Op::Command(CommandId::FileSave),
            Op::SaveAs(p("out.md")),
            Op::Command(CommandId::FormatBold),
            Op::Type("Hello\nWorld\t!\\".to_owned()),
            Op::Key("ctrl+shift+z".parse().unwrap()),
            Op::Select { anchor: 3, head: 7 },
            Op::SelectText("two words".to_owned()),
            Op::Answer(
                DialogKind::MessageBox,
                Answer::Button(MessageBoxResult::DontSave),
            ),
            Op::Answer(
                DialogKind::OpenFile,
                Answer::Paths(vec![p("a.md"), p("b c.md")]),
            ),
            Op::Answer(DialogKind::SaveFile, Answer::Cancel),
            Op::ClipboardText("a\tb".to_owned()),
            Op::ClipboardHtml(p("page.html")),
            Op::ClipboardImage(p("pic.png")),
            Op::Paste { plain: false },
            Op::Paste { plain: true },
            Op::ClipboardText("plain".to_owned()),
            Op::Paste { plain: false },
            Op::ClipboardHtml(p("page.html")),
            Op::Paste { plain: false },
            Op::ClipboardImage(p("pic.png")),
            Op::Paste { plain: false },
            Op::Tab(2),
            Op::Advance(Duration::from_millis(1500)),
            Op::Sleep(Duration::from_millis(20)),
            Op::WaitIdle,
            Op::WaitExternal(DEFAULT_EXTERNAL_WAIT),
            Op::WaitExternal(Duration::from_millis(250)),
            Op::ExternalWrite {
                path: p("x y.md"),
                text: "new\ntext".to_owned(),
            },
            Op::AssertText(p("expected.md")),
            Op::AssertTextIs("two\nlines".to_owned()),
            Op::AssertFile {
                path: p("out.md"),
                expected: p("expected.md"),
            },
            Op::AssertExists(p("a.md")),
            Op::AssertNoFile(p("b.md")),
            Op::AssertMagic {
                path: p("out.docx"),
                magic: Magic::Docx,
            },
            Op::AssertDirty(false),
            Op::AssertTabs(2),
            Op::AssertSelection { anchor: 1, head: 2 },
            Op::AssertClipboard(p("clip.txt")),
            Op::AssertDialog {
                kind: DialogKind::MessageBox,
                contains: "Unsaved changes".to_owned(),
            },
            Op::DumpState(p("state.json")),
            Op::Theme(ThemeChoice::Dark),
            Op::Theme(ThemeChoice::Light),
            Op::Screenshot(shot("shot.png", 1440, 900, 100)),
            Op::Screenshot(shot("my shot.png", 1440, 900, 200)),
            Op::Screenshot(shot("a.png", 800, 600, 100)),
            Op::Screenshot(shot("a.png", 800, 600, 150)),
            Op::SettingsPage(SettingsSection::Appearance),
            Op::ScreenshotSettings(shot("s.png", 900, 640, 100)),
            Op::ScreenshotSettings(shot("s.png", 450, 320, 200)),
            Op::Quit,
        ];
        assert_eq!(ops, expected);
        // `paste-text` is two steps of one line.
        let lines: Vec<usize> = script.steps().iter().map(|step| step.line).collect();
        assert_eq!(lines[17], lines[18]);
        assert_eq!(lines[19], lines[20]);
    }

    #[test]
    fn invalid_lines_are_reported_with_their_line_number() {
        for (line, message) in [
            ("frobnicate", "unknown step `frobnicate`"),
            ("open", "open takes 1 argument(s), found 0"),
            ("open a b", "found 2"),
            ("open \"a", "a quote is not closed"),
            ("new now", "new takes no arguments"),
            ("type", "type needs text"),
            ("type bad\\q", "unknown escape `\\q`"),
            ("type end\\", "the text ends with a backslash"),
            ("command no.such", "unknown command id `no.such`"),
            ("key ctrl+nope", "invalid key chord"),
            ("select 1", "select takes 2"),
            ("select a b", "select needs a number, found `a`"),
            (
                "advance soon",
                "advance needs a number of milliseconds, found `soon`",
            ),
            ("wait-external 1 2", "at most one"),
            ("external-write", "needs a path and a text"),
            ("answer", "answer needs a dialog kind and a choice"),
            ("answer popup ok", "unknown dialog kind `popup`"),
            ("answer open-file", "answer needs a choice"),
            ("answer message-box ok ok", "one button"),
            ("answer message-box maybe", "unknown button `maybe`"),
            ("assert-magic a.bin elf", "unknown file kind `elf`"),
            ("assert-dirty maybe", "needs true or false, found `maybe`"),
            ("assert-dialog", "needs a dialog kind and a text"),
            ("assert-dialog message-box", "assert-dialog needs text"),
            ("theme", "theme takes 1 argument(s), found 0"),
            ("theme blue", "unknown theme `blue` (light or dark)"),
            ("screenshot", "screenshot needs a path"),
            ("screenshot-settings", "screenshot-settings needs a path"),
            (
                "screenshot a.png 800",
                "screenshot needs a scale between 0.5 and 4, found `800`",
            ),
            (
                "screenshot a.png 10 600",
                "needs a size between 64 and 8192 DIPs, found `10`",
            ),
            ("screenshot a.png 800 9000", "found `9000`"),
            ("screenshot a.png 800 600 5", "found `5`"),
            ("screenshot a.png 800 600 big", "found `big`"),
            ("screenshot a.png 1 2 3 4", "each optional"),
            (
                "settings-page", // docs-check: not a message id
                "settings-page takes 1 argument(s), found 0",
            ),
            ("settings-page colors", "unknown settings page `colors`"),
            ("open \"a b\\\"", "a quote is not closed"),
            ("save now", "save takes no arguments"),
            ("save-as", "save-as takes 1 argument(s), found 0"),
            ("command", "command takes 1 argument(s), found 0"),
            ("key", "key takes 1 argument(s), found 0"),
            ("select 1 b", "select needs a number, found `b`"),
            ("select-text", "select-text needs text"),
            ("clipboard-text", "clipboard-text needs text"), // docs-check: not a message id
            (
                "clipboard-html", // docs-check: not a message id
                "clipboard-html takes 1 argument(s), found 0",
            ),
            (
                "clipboard-image", // docs-check: not a message id
                "clipboard-image takes 1 argument(s), found 0",
            ),
            ("paste now", "paste takes no arguments"),
            ("paste-text", "paste-text needs text"),
            ("paste-html", "paste-html takes 1 argument(s), found 0"),
            ("paste-image", "paste-image takes 1 argument(s), found 0"),
            ("tab", "tab takes 1 argument(s), found 0"),
            ("tab x", "tab needs a number, found `x`"),
            ("advance", "advance takes 1 argument(s), found 0"),
            ("sleep", "sleep takes 1 argument(s), found 0"),
            (
                "sleep soon",
                "sleep needs a number of milliseconds, found `soon`",
            ),
            ("wait-idle now", "wait-idle takes no arguments"),
            ("wait-external \"a", "a quote is not closed"),
            (
                "wait-external soon",
                "needs a number of milliseconds, found `soon`",
            ),
            ("external-write \"a", "a quote is not closed"),
            ("external-write a.md bad\\q", "unknown escape `\\q`"),
            ("assert-text", "assert-text takes 1 argument(s), found 0"),
            ("assert-text-is bad\\q", "unknown escape `\\q`"),
            (
                "assert-file a.md",
                "assert-file takes 2 argument(s), found 1",
            ),
            (
                "assert-exists",
                "assert-exists takes 1 argument(s), found 0",
            ),
            (
                "assert-no-file",
                "assert-no-file takes 1 argument(s), found 0",
            ),
            (
                "assert-magic a.bin",
                "assert-magic takes 2 argument(s), found 1",
            ),
            ("assert-dirty", "assert-dirty takes 1 argument(s), found 0"),
            ("assert-tabs", "assert-tabs takes 1 argument(s), found 0"),
            ("assert-tabs x", "assert-tabs needs a number, found `x`"),
            (
                "assert-selection 1",
                "assert-selection takes 2 argument(s), found 1",
            ),
            ("assert-selection a 1", "needs a number, found `a`"),
            ("assert-selection 1 b", "needs a number, found `b`"),
            (
                "assert-clipboard",
                "assert-clipboard takes 1 argument(s), found 0",
            ),
            ("assert-dialog \"a", "a quote is not closed"),
            ("assert-dialog popup text", "unknown dialog kind `popup`"),
            ("dump-state", "dump-state takes 1 argument(s), found 0"),
            ("quit now", "quit takes no arguments"),
            ("screenshot \"a", "a quote is not closed"),
            (
                "screenshot a.png 10 600 2",
                "needs a size between 64 and 8192 DIPs, found `10`",
            ),
            ("screenshot a.png 800 9000 2", "found `9000`"),
            ("answer open-file \"a", "a quote is not closed"),
        ] {
            // `Script::parse` only fails with syntax errors; the display below proves it.
            let (at, found) = located(Script::parse(&format!("\nnew\n{line}")).unwrap_err());
            assert_eq!(at, 3, "{line}");
            assert!(found.contains(message), "{line}: {found}");
            assert_eq!(
                ScriptError::Syntax {
                    line: at,
                    message: found.clone()
                }
                .to_string(),
                format!("line 3: {found}")
            );
        }
    }

    #[test]
    fn every_message_box_button_and_the_carriage_return_escape_parse() {
        let script = Script::parse(
            "answer message-box ok\nanswer message-box cancel\nanswer message-box yes\n\
             answer message-box no\nanswer message-box save\ntype a\\rb\n",
        )
        .unwrap();
        let ops: Vec<Op> = script.steps().iter().map(|step| step.op.clone()).collect();
        let button = |button| Op::Answer(DialogKind::MessageBox, Answer::Button(button));
        assert_eq!(
            ops,
            [
                button(MessageBoxResult::Ok),
                button(MessageBoxResult::Cancel),
                button(MessageBoxResult::Yes),
                button(MessageBoxResult::No),
                button(MessageBoxResult::Save),
                Op::Type("a\rb".to_owned()),
            ]
        );
    }

    #[test]
    fn ctrl_alt_chords_are_shortcuts_because_the_harness_has_no_altgr() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ran(
            tmp.path(),
            "new\ntype keep\nkey ctrl+alt+q\nassert-text-is keep\n",
        );
        runner.finish();
    }

    #[test]
    fn a_close_request_of_the_settings_window_closes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ran(tmp.path(), "command settings.open\n");
        assert!(runner.controller().settings_cell().is_some());
        runner.controller().push(Request::CloseSettings);
        runner.run(&Script::parse("new\n").unwrap()).unwrap();
        assert!(runner.controller().settings_cell().is_none());
        runner.finish();
    }

    #[test]
    fn files_that_differ_only_in_length_differ_where_the_shorter_one_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let (short, long) = (tmp.path().join("short.txt"), tmp.path().join("long.txt"));
        std::fs::write(&short, "abc").unwrap();
        std::fs::write(&long, "abcd").unwrap();
        let script = format!("assert-file {} {}\n", quoted(&short), quoted(&long));
        let (line, message) = failure(run(tmp.path(), &script));
        assert_eq!(line, 1);
        assert!(
            message.contains("3 bytes against 4 bytes, first difference at byte 3"),
            "{message}"
        );
    }

    #[test]
    fn files_are_recognized_by_their_first_bytes() {
        let zip = |inner: &[u8]| {
            let mut bytes = b"PK\x03\x04".to_vec();
            bytes.extend_from_slice(inner);
            bytes
        };
        assert!(Magic::Zip.matches(&zip(b"anything")));
        assert!(Magic::Docx.matches(&zip(b"..word/document.xml..")));
        assert!(!Magic::Docx.matches(&zip(b"content.xml")));
        assert!(Magic::Odt.matches(&zip(b"mimetypeapplication/vnd.oasis.opendocument.text")));
        assert!(Magic::Epub.matches(&zip(b"mimetypeapplication/epub+zip")));
        assert!(!Magic::Zip.matches(b"%PDF-1.7"));
        assert!(Magic::Pdf.matches(b"%PDF-1.7\n"));
        assert!(Magic::Rtf.matches(b"{\\rtf1\\ansi"));
        assert!(Magic::Html.matches(b"<!DOCTYPE html><title>x</title>"));
        assert!(Magic::Html.matches(b"  <HTML><body>"));
        assert!(!Magic::Html.matches(b"plain text"));
        assert!(Magic::Png.matches(&[0x89, b'P', b'N', b'G', 13, 10]));
        assert!(!Magic::Png.matches(b"GIF89a"));
        assert!(!Magic::Pdf.matches(b""));
        for name in ["zip", "docx", "odt", "epub", "pdf", "rtf", "html", "png"] {
            assert!(Magic::from_name(name).is_some(), "{name}");
        }
    }

    #[test]
    fn typing_saving_and_dumping_the_state_leave_real_bytes_and_json() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out.md");
        let expected = tmp.path().join("expected.md");
        let state = tmp.path().join("state.json");
        std::fs::write(&expected, "first line\nsecond ü line").unwrap();
        let script = format!(
            "new\ntype first line\\nsecond ü line\nassert-dirty true\nassert-text {expected}\n\
             save-as {out}\nassert-dirty false\nassert-file {out} {expected}\n\
             assert-dialog save-file Save as\ndump-state {state}\n",
            expected = quoted(&expected),
            out = quoted(&out),
            state = quoted(&state),
        );
        run(tmp.path(), &script).unwrap();
        assert_eq!(
            std::fs::read(&out).unwrap(),
            "first line\nsecond ü line".as_bytes()
        );
        let dump: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        let tabs = dump["windows"][0]["tabs"].as_array().unwrap();
        let saved = tabs.last().unwrap();
        assert_eq!(saved["text"], "first line\nsecond ü line");
        assert_eq!(saved["dirty"], false);
        assert_eq!(saved["encoding"], "UTF-8");
        assert_eq!(saved["line_ending"], "Lf");
        assert!(saved["path"].as_str().unwrap().ends_with("out.md"));
        assert_eq!(dump["dialogs"][0]["kind"], "save-file");
        assert_eq!(dump["dialogs"][0]["answered"], true);
        assert!(
            dump["settings_path"]
                .as_str()
                .unwrap()
                .ends_with("settings.json")
        );
        assert_eq!(dump["autosave"], true);
        assert_eq!(dump["windows"][0]["active"], true);
        // The session of the clean exit was stored with the saved tab.
        let session = core(tmp.path()).plan;
        assert!(session.windows[0].tabs.iter().any(|tab| matches!(
            &tab.document,
            crate::session::PlannedDocument::File(path) if path.ends_with("out.md")
        )));
    }

    #[test]
    fn shortcuts_selection_and_undo_run_through_the_real_editor() {
        let tmp = tempfile::tempdir().unwrap();
        let script = "new\n\
            type hello world\n\
            select-text world\n\
            key ctrl+b\n\
            assert-text-is hello **world**\n\
            key ctrl+z\n\
            assert-text-is hello world\n\
            select 0 5\n\
            assert-selection 0 5\n\
            command edit.select_all\n\
            assert-selection 0 11\n\
            advance 1500\n\
            key ctrl+shift+z\n\
            assert-text-is hello **world**\n";
        run(tmp.path(), script).unwrap();
    }

    #[test]
    fn selection_offsets_must_be_character_boundaries_inside_the_text() {
        let tmp = tempfile::tempdir().unwrap();
        let (line, message) = failure(run(tmp.path(), "new\ntype ü\nselect 1 1\n"));
        assert_eq!(line, 3);
        assert!(message.contains("not a character boundary"), "{message}");
        let (_, message) = failure(run(&tmp.path().join("b"), "new\ntype ü\nselect 0 9\n"));
        assert!(message.contains("not a character boundary"), "{message}");
        let (_, message) = failure(run(&tmp.path().join("c"), "new\ntype ü\nselect-text zzz\n"));
        assert!(message.contains("does not contain \"zzz\""), "{message}");
    }

    #[test]
    fn failing_assertions_name_the_line_and_the_difference() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.bin");
        let b = tmp.path().join("b.bin");
        std::fs::write(&a, b"abcd").unwrap();
        std::fs::write(&b, b"abXd!").unwrap();
        let missing = tmp.path().join("missing.md");
        let cases = [
            (
                "assert-text-is nope".to_owned(),
                "the document text differs",
            ),
            (
                format!("assert-file {} {}", quoted(&a), quoted(&b)),
                "first difference at byte 2",
            ),
            (
                format!(
                    "assert-file {} {}",
                    quoted(&a),
                    quoted(&a.with_extension("x"))
                ),
                "cannot read",
            ),
            (
                format!("assert-exists {}", quoted(&missing)),
                "does not exist",
            ),
            (format!("assert-no-file {}", quoted(&a)), "exists"),
            (
                format!("assert-magic {} pdf", quoted(&a)),
                "is not a Pdf file",
            ),
            ("assert-dirty true".to_owned(), "the document is clean"),
            (
                "assert-tabs 5".to_owned(),
                "the window has 2 tab(s), expected 5",
            ),
            ("assert-selection 1 2".to_owned(), "the selection is 0..0"),
            (
                format!("assert-clipboard {}", quoted(&a)),
                "the clipboard text differs",
            ),
            (
                "assert-dialog message-box nothing".to_owned(),
                "no message-box dialog mentioned",
            ),
            ("tab 4".to_owned(), "the window has no tab 4"),
            ("key f10".to_owned(), "opens a menu"),
            (
                format!("clipboard-html {}", quoted(&missing)),
                "cannot read",
            ),
            (
                format!("clipboard-image {}", quoted(&missing)),
                "cannot read",
            ),
            (format!("assert-text {}", quoted(&missing)), "cannot read"),
            (
                format!("assert-file {} {}", quoted(&missing), quoted(&a)),
                "cannot read",
            ),
            (
                format!("assert-magic {} pdf", quoted(&missing)),
                "cannot read",
            ),
            (
                format!("assert-clipboard {}", quoted(&missing)),
                "cannot read",
            ),
            (format!("open {}", quoted(&missing)), "cannot open"),
            (
                format!(
                    "external-write {} x",
                    quoted(&tmp.path().join("no").join("dir.md"))
                ),
                "cannot write",
            ),
            (
                format!(
                    "dump-state {}",
                    quoted(&tmp.path().join("no").join("state.json"))
                ),
                "cannot write",
            ),
            (
                "wait-external 30".to_owned(),
                "an external change did not happen within 30 ms",
            ),
        ];
        for (index, (step, expected)) in cases.into_iter().enumerate() {
            let profile = tmp.path().join(format!("case{index}"));
            let (line, message) = failure(run(&profile, &format!("new\n{step}\n")));
            assert_eq!(line, 2, "{step}");
            assert!(message.contains(expected), "{step}: {message}");
        }
    }

    #[test]
    fn a_dialog_nobody_answered_fails_and_an_answer_nobody_used_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = |name: &str| {
            let root = tmp.path().join(name);
            settings(&root, r#"{"general":{"autosave":false}}"#);
            root
        };
        let (line, message) = failure(run(
            &profile("unanswered"),
            "new\ntype draft\ncommand tab.close\n",
        ));
        assert_eq!(line, 3);
        assert!(
            message.contains("unexpected dialog: message-box"),
            "{message}"
        );
        assert!(message.contains("Unsaved changes"), "{message}");

        // Answered: the user presses Don't save and the tab closes.
        run(
            &profile("answered"),
            "new\ntype draft\nanswer message-box dont-save\ncommand tab.close\n\
             assert-dialog message-box Untitled\nassert-dirty false\n",
        )
        .unwrap();

        let (line, message) = failure(run(&profile("folder"), "new\nanswer pick-folder cancel\n"));
        assert_eq!(line, 2);
        assert!(
            message.contains("no pick-folder dialog asked for the answer Cancel"),
            "{message}"
        );

        let (line, message) = failure(run(&profile("open"), "answer open-file x.md\n"));
        assert_eq!(line, 1);
        assert!(message.contains("no open-file dialog"), "{message}");
    }

    #[test]
    fn save_as_fails_when_an_earlier_answer_took_the_dialog() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first.md");
        let second = tmp.path().join("second.md");
        // The queued answer `first` is consumed by the dialog `save-as second` provokes; the
        // answer for `second` stays unused, so the step reports that its dialog never came.
        let script = format!(
            "new\ntype x\nanswer save-file {first}\nsave-as {second}\n",
            first = quoted(&first),
            second = quoted(&second),
        );
        let (line, message) = failure(run(tmp.path(), &script));
        assert_eq!(line, 4);
        assert!(
            message.contains("save as showed no save dialog"),
            "{message}"
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "x");
        assert!(!second.exists());
    }

    #[test]
    fn pasting_puts_the_prepared_content_on_the_clipboard_first() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = tmp.path().join("clip.txt");
        std::fs::write(&expected, "from the clipboard").unwrap();
        let script = format!(
            "new\npaste-text from the clipboard\nassert-text-is from the clipboard\n\
             assert-clipboard {expected}\nclipboard-text more\npaste-plain\n\
             assert-text-is from the clipboardmore\n",
            expected = quoted(&expected)
        );
        run(tmp.path(), &script).unwrap();
    }

    #[test]
    fn quit_ends_the_script_and_opening_files_adds_tabs_after_replacing_the_unused_one() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.md");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, "# A\n").unwrap();
        std::fs::write(&b, "plain\n").unwrap();
        let script = format!(
            "open {a}\nopen {b}\nassert-tabs 2\ntab 1\nassert-text-is # A\\n\ntab 2\n\
             assert-text-is plain\\n\nopen {a}\nassert-tabs 2\nassert-text-is # A\\n\n\
             quit\nassert-text-is never reached\n",
            a = quoted(&a),
            b = quoted(&b),
        );
        run(tmp.path(), &script).unwrap();
    }

    #[test]
    fn autosave_writes_the_file_after_the_delay() {
        let tmp = tempfile::tempdir().unwrap();
        settings(tmp.path(), r#"{"general":{"autosave_delay_ms":400}}"#);
        let file = tmp.path().join("auto.md");
        std::fs::write(&file, "start\n").unwrap();
        let start = tmp.path().join("start.md");
        std::fs::write(&start, "start\n").unwrap();
        let expected = tmp.path().join("expected.md");
        std::fs::write(&expected, "start\nmore").unwrap();
        let script = format!(
            "open {file}\nkey ctrl+end\ntype more\nassert-dirty true\nassert-file {file} {start}\n\
             sleep 600\nassert-dirty false\nassert-file {file} {expected}\n",
            file = quoted(&file),
            start = quoted(&start),
            expected = quoted(&expected),
        );
        run(tmp.path(), &script).unwrap();
    }

    #[test]
    fn an_external_write_reloads_the_document() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("watched.md");
        std::fs::write(&file, "one\n").unwrap();
        let script = format!(
            "open {file}\nexternal-write {file} two\\nthree\\n\nwait-external 10000\n\
             assert-text-is two\\nthree\\n\nassert-dirty false\nwait-idle\n",
            file = quoted(&file)
        );
        run(tmp.path(), &script).unwrap();
    }

    #[test]
    fn the_host_reports_unreadable_invalid_failing_and_passing_scripts_by_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let args = |script: Option<PathBuf>| Args {
            test: crate::args::TestFlags {
                hidden: true,
                script,
                ..crate::args::TestFlags::default()
            },
            ..Args::default()
        };
        let mut host = ScriptHost;
        assert_eq!(
            host.run(core(&tmp.path().join("a")), &args(None)),
            EXIT_SCRIPT_UNREADABLE
        );
        assert_eq!(
            host.run(
                core(&tmp.path().join("b")),
                &args(Some(tmp.path().join("missing.txt")))
            ),
            EXIT_SCRIPT_UNREADABLE
        );
        let script = tmp.path().join("script.txt");
        std::fs::write(&script, "frobnicate\n").unwrap();
        assert_eq!(
            host.run(core(&tmp.path().join("c")), &args(Some(script.clone()))),
            EXIT_SCRIPT_INVALID
        );
        std::fs::write(&script, "new\nassert-text-is something else\n").unwrap();
        assert_eq!(
            host.run(core(&tmp.path().join("d")), &args(Some(script.clone()))),
            EXIT_SCRIPT_FAILED
        );
        std::fs::write(&script, "new\ntype ok\nassert-text-is ok\n").unwrap();
        assert_eq!(
            host.run(core(&tmp.path().join("e")), &args(Some(script))),
            0
        );
    }

    #[test]
    fn wait_idle_fails_when_a_worker_does_not_finish() {
        use crate::workers::{JobKey, JobKind, JobSpec};

        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ScriptRunner::start(core(tmp.path()));
        let spec = JobSpec {
            kind: JobKind::Io,
            window: None,
            key: JobKey(1),
            generation: 1,
        };
        runner
            .controller()
            .core()
            .borrow()
            .workers
            .submit(
                spec,
                |_| std::thread::sleep(Duration::from_millis(600)),
                |_| {},
            )
            .unwrap();
        runner.set_wait_limit(Duration::from_millis(50));
        let result = runner.run(&Script::parse("wait-idle\n").unwrap());
        let (line, message) = failure(result);
        assert_eq!(line, 1);
        assert!(message.contains("did not happen within 50 ms"), "{message}");
        // Given time, the same wait succeeds.
        runner.set_wait_limit(Duration::from_secs(30));
        runner.run(&Script::parse("wait-idle\n").unwrap()).unwrap();
        runner.finish();
    }

    #[test]
    fn steps_need_a_window_and_a_document() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ScriptRunner::start(core(tmp.path()));
        let window = runner.controller().active_window().unwrap();
        let key = window.view.borrow().active_tab().unwrap().key;
        window.view.borrow_mut().close_tab(key).unwrap();
        let expected = tmp.path().join("expected.md");
        std::fs::write(&expected, "text\n").unwrap();
        let no_document = [
            "assert-dirty true".to_owned(),
            "assert-selection 0 0".to_owned(),
            "select 0 0".to_owned(),
            "select-text x".to_owned(),
            "assert-text-is ".to_owned(),
            format!("assert-text {}", quoted(&expected)),
        ];
        for step in no_document {
            let (line, message) =
                failure(runner.run(&Script::parse(&format!("{step}\n")).unwrap()));
            assert_eq!(line, 1, "{step}");
            assert!(message.contains("has no document"), "{step}: {message}");
        }
        // Without a document nobody handles a command of the document.
        let (line, message) =
            failure(runner.run(&Script::parse("command edit.select_all\n").unwrap()));
        assert_eq!(line, 1);
        assert!(message.ends_with(" was not handled"), "{message}");
        runner.controller().close_window(window.id);
        for step in [
            "new",
            "type x",
            "type \\n",
            "type \\t",
            "assert-tabs 1",
            "tab 1",
            "open x.md",
            "assert-text-is ",
            "save-as x.md",
            "select 0 0",
            "select-text x",
        ] {
            let (_, message) = failure(runner.run(&Script::parse(&format!("{step}\n")).unwrap()));
            assert!(message.contains("has no window"), "{step}: {message}");
        }
        runner.finish();
    }

    #[test]
    fn queued_requests_are_carried_out_like_the_message_loop_does() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state.json");
        let mut runner = ScriptRunner::start(core(tmp.path()));
        let id = runner.controller().active_window().unwrap().id;
        let controller = Rc::clone(runner.controller());
        // A command for a window that does not exist is nobody's: the dump says so.
        controller.push(Request::Command(
            crate::workers::WindowId(9_999),
            CommandId::FileNew,
        ));
        controller.push(Request::Command(id, CommandId::FileNew));
        controller.push(Request::Action(
            id,
            ShellAction::Title(TitleEvent::ToggleSidebar),
        ));
        controller.push(Request::CloseRequested(id));
        let script = format!("assert-tabs 2\ndump-state {}\n", quoted(&state));
        runner.run(&Script::parse(&script).unwrap()).unwrap();
        runner.finish();
        let dump: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(dump["windows"][0]["sidebar_visible"], true);
        let unhandled = dump["unhandled_requests"].as_array().unwrap();
        assert_eq!(unhandled.len(), 2, "{unhandled:?}");
        assert!(unhandled[0].as_str().unwrap().starts_with("command "));
        assert!(unhandled[1].as_str().unwrap().contains("CloseRequested"));
    }

    #[test]
    fn an_autosave_that_fails_is_noted_in_the_state_dump() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        settings(tmp.path(), r#"{"general":{"autosave_delay_ms":200}}"#);
        let file = tmp.path().join("locked.md");
        std::fs::write(&file, "start\n").unwrap();
        let state = tmp.path().join("state.json");
        let mut runner = ScriptRunner::start(core(tmp.path()));
        let edit = format!("open {}\nkey ctrl+end\ntype more\n", quoted(&file));
        runner.run(&Script::parse(&edit).unwrap()).unwrap();
        // Another program takes the file before the autosave delay is over. The tick tries
        // once per turn and waits 50, 100, 200, 400 and 800 ms between the tries; the failure
        // is noted when the last try fails. Every pause outlasts the wait it follows.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let pauses = "sleep 300\nsleep 60\nsleep 110\nsleep 210\nsleep 410\nsleep 810\n";
        let wait = format!("{pauses}dump-state {}\n", quoted(&state));
        runner.run(&Script::parse(&wait).unwrap()).unwrap();
        drop(lock);
        runner.finish();
        let dump: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        let errors = dump["autosave_errors"].as_array().unwrap();
        assert!(!errors.is_empty(), "{dump}");
        assert!(!errors[0].as_str().unwrap().is_empty());
    }

    /// The pixel size from the header of a PNG file and the number of distinct colors.
    fn png_facts(path: &Path) -> ((u32, u32), usize) {
        let bytes = std::fs::read(path).unwrap();
        assert!(Magic::Png.matches(&bytes), "{path:?} is a PNG");
        let side = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
        let image = ssw_win::wic::Wic::new().unwrap().decode(&bytes).unwrap();
        let colors: std::collections::HashSet<[u8; 4]> = (0..image.height)
            .flat_map(|y| (0..image.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| image.pixel(x, y))
            .collect();
        ((side(16), side(20)), colors.len())
    }

    #[test]
    fn screenshots_paint_the_window_and_the_settings_window_offscreen() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let window = tmp.path().join("shots").join("window.png");
        let hidpi = tmp.path().join("hidpi.png");
        let settings = tmp.path().join("settings.png");
        run(
            tmp.path(),
            &format!(
                "theme dark\nnew\ntype # Title\\n\\nSome **bold** text and $x^2$.\\n\n\
                 screenshot {window} 320 200\nscreenshot {hidpi} 160 100 2\n\
                 command settings.open\nsettings-page editor\n\
                 screenshot-settings {settings} 450 320\n",
                window = quoted(&window),
                hidpi = quoted(&hidpi),
                settings = quoted(&settings),
            ),
        )
        .unwrap();
        let (size, colors) = png_facts(&window);
        assert_eq!(size, (320, 200));
        assert!(
            colors > 8,
            "the window is drawn, not a flat color: {colors}"
        );
        assert_eq!(png_facts(&hidpi).0, (320, 200), "160x100 DIPs at scale 2");
        let (size, colors) = png_facts(&settings);
        assert_eq!(size, (450, 320));
        assert!(colors > 8, "{colors}");
    }

    #[test]
    fn the_settings_steps_need_the_settings_window() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (line, message) = failure(run(tmp.path(), "new\nsettings-page general\n"));
        assert_eq!(
            (line, message.contains("Settings window is not open")),
            (2, true)
        );
        let shot = tmp.path().join("s.png");
        let (line, message) = failure(run(
            tmp.path(),
            &format!("screenshot-settings {}\n", quoted(&shot)),
        ));
        assert_eq!((line, message.contains("not open")), (1, true));
        assert!(!shot.exists());
    }

    #[test]
    fn a_screenshot_too_large_for_a_bitmap_or_into_a_missing_drive_fails() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let shot = tmp.path().join("big.png");
        let (line, message) = failure(run(
            tmp.path(),
            &format!("new\nscreenshot {} 8192 8192 4\n", quoted(&shot)),
        ));
        assert_eq!(line, 2);
        assert!(message.contains("larger than"), "{message}");
        let blocked = tmp.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        let under_a_file = blocked.join("shot.png");
        let (_, message) = failure(run(
            tmp.path(),
            &format!("screenshot {} 100 100\n", quoted(&under_a_file)),
        ));
        assert!(message.contains("cannot write"), "{message}");
    }

    #[test]
    fn the_runner_debug_shows_its_controller_and_whether_it_quit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut runner = ScriptRunner::start(core(tmp.path()));
        let before = format!("{runner:?}");
        assert!(
            before.starts_with("ScriptRunner { controller: "),
            "{before}"
        );
        assert!(before.ends_with(", quit: false, .. }"), "{before}");
        runner.run(&Script::parse("quit\n").unwrap()).unwrap();
        let after = format!("{runner:?}");
        assert!(after.ends_with(", quit: true, .. }"), "{after}");
        runner.finish();
    }

    #[test]
    fn a_request_from_the_settings_window_is_carried_out_like_the_message_loop_does() {
        use ssw_ui::shell::settings::SettingsEvent;

        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state.json");
        let mut runner = ScriptRunner::start(core(tmp.path()));
        // The user pressed "Default apps" in the Settings window.
        runner
            .controller()
            .push(Request::Settings(SettingsEvent::OpenDefaultApps));
        let script = format!("dump-state {}\n", quoted(&state));
        runner.run(&Script::parse(&script).unwrap()).unwrap();
        runner.finish();
        let dump: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(dump["shell"], json!(["OpenDefaultApps"]));
    }

    #[test]
    fn passing_file_assertions_and_the_dirty_state_a_failing_one_names() {
        let tmp = tempfile::tempdir().unwrap();
        let pdf = tmp.path().join("doc.pdf");
        std::fs::write(&pdf, b"%PDF-1.7\n").unwrap();
        let absent = tmp.path().join("absent.bin");
        let script = format!(
            "assert-exists {pdf}\nassert-no-file {absent}\nassert-magic {pdf} pdf\n",
            pdf = quoted(&pdf),
            absent = quoted(&absent),
        );
        run(tmp.path(), &script).unwrap();
        let (line, message) = failure(run(
            &tmp.path().join("typed"),
            "new\ntype x\nassert-dirty false\n",
        ));
        assert_eq!(line, 3);
        assert!(message.contains("the document is dirty"), "{message}");
    }

    #[test]
    fn saving_a_document_another_program_holds_fails_the_step_naming_the_command() {
        use std::os::windows::fs::OpenOptionsExt;

        let tmp = tempfile::tempdir().unwrap();
        settings(tmp.path(), r#"{"general":{"autosave":false}}"#);
        let file = tmp.path().join("held.md");
        std::fs::write(&file, "start\n").unwrap();
        let mut runner = ScriptRunner::start(core(tmp.path()));
        let edit = format!("open {}\nkey ctrl+end\ntype more\n", quoted(&file));
        runner.run(&Script::parse(&edit).unwrap()).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let (line, message) = failure(
            runner.run(&Script::parse("answer message-box ok\ncommand file.save\n").unwrap()),
        );
        drop(lock);
        assert_eq!(line, 2);
        assert!(message.starts_with("file.save failed: "), "{message}");
        runner.finish();
    }

    #[test]
    fn a_typed_tab_is_a_tab_key_of_the_editor() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), "new\ntype a\\tb\nassert-text-is a\\tb\n").unwrap();
    }

    #[test]
    fn opening_a_folder_makes_it_the_workspace_of_the_tab_in_the_state_dump() {
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("notes");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("a.md"), "# A\n").unwrap();
        let state = tmp.path().join("state.json");
        let script = format!(
            "new\nanswer pick-folder {folder}\ncommand file.open_folder\ndump-state {state}\n",
            folder = quoted(&folder),
            state = quoted(&state),
        );
        run(tmp.path(), &script).unwrap();
        let dump: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        let window = &dump["windows"][0];
        assert_eq!(window["workspace_shown"], true);
        let tabs = window["tabs"].as_array().unwrap();
        let active = tabs.iter().find(|tab| tab["active"] == true).unwrap();
        assert_eq!(active["workspace"], folder.to_string_lossy().as_ref());
    }

    #[test]
    fn prepared_html_and_pictures_are_pasted_as_markdown_and_as_saved_files() {
        use image::codecs::png::PngEncoder;
        use image::{ExtendedColorType, ImageEncoder};

        let tmp = tempfile::tempdir().unwrap();
        let page = tmp.path().join("page.html");
        std::fs::write(&page, "<p>Hello <b>world</b></p>").unwrap();
        let notes = tmp.path().join("notes.md");
        std::fs::write(&notes, "# Pics\n").unwrap();
        let mut picture = Vec::new();
        PngEncoder::new(&mut picture)
            .write_image(
                &[200, 30, 30, 255].repeat(64),
                8,
                8,
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        let pic = tmp.path().join("pic.png");
        std::fs::write(&pic, &picture).unwrap();
        let script = format!(
            "new\nclipboard-text Hello world\nclipboard-html {page}\npaste\n\
             assert-text-is Hello **world**\n\
             open {notes}\nkey ctrl+end\nclipboard-image {pic}\npaste\n\
             assert-text-is # Pics\\n![](image-1.png)\n\
             assert-file {saved} {pic}\n",
            page = quoted(&page),
            notes = quoted(&notes),
            pic = quoted(&pic),
            saved = quoted(&tmp.path().join("image-1.png")),
        );
        run(tmp.path(), &script).unwrap();
    }

    /// The sum of the color channels of the pixel at (2, 2) of a PNG file: the window color.
    fn corner_brightness(path: &Path) -> u32 {
        let bytes = std::fs::read(path).unwrap();
        let image = ssw_win::wic::Wic::new().unwrap().decode(&bytes).unwrap();
        let [b, g, r, _] = image.pixel(2, 2).unwrap();
        u32::from(b) + u32::from(g) + u32::from(r)
    }

    /// How many pixels of a PNG file are clearly red.
    fn red_pixels(path: &Path) -> usize {
        let bytes = std::fs::read(path).unwrap();
        let image = ssw_win::wic::Wic::new().unwrap().decode(&bytes).unwrap();
        (0..image.height)
            .flat_map(|y| (0..image.width).map(move |x| (x, y)))
            .filter_map(|(x, y)| image.pixel(x, y))
            .filter(|&[b, g, r, _]| r > 200 && g < 60 && b < 60)
            .count()
    }

    #[test]
    fn the_theme_step_recolors_an_open_settings_window() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (light, dark) = (tmp.path().join("light.png"), tmp.path().join("dark.png"));
        let script = format!(
            "command settings.open\ntheme light\nscreenshot-settings {light} 200 120\n\
             theme dark\nscreenshot-settings {dark} 200 120\n",
            light = quoted(&light),
            dark = quoted(&dark),
        );
        run(tmp.path(), &script).unwrap();
        assert!(corner_brightness(&light) > 600, "light window color");
        assert!(corner_brightness(&dark) < 250, "dark window color");
    }

    #[test]
    fn a_change_the_settings_window_reports_during_a_screenshot_is_carried_out() {
        use ssw_ui::shell::settings::FieldId;

        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let folder = tmp.path().join("docs");
        std::fs::create_dir_all(&folder).unwrap();
        let folder = folder.to_string_lossy().into_owned();
        let mut runner = ran(tmp.path(), "command settings.open\n");
        let cell = runner.controller().settings_cell().unwrap();
        // A folder picked in the window and not yet carried out by the controller.
        cell.view
            .borrow_mut()
            .window_mut()
            .set_folder(FieldId::DefaultFolder, &folder);
        let shot = tmp.path().join("s.png");
        let script = format!("screenshot-settings {} 200 120\n", quoted(&shot));
        runner.run(&Script::parse(&script).unwrap()).unwrap();
        assert_eq!(
            runner
                .controller()
                .settings()
                .general
                .default_folder
                .as_deref(),
            Some(folder.as_str())
        );
        runner.finish();
    }

    #[test]
    fn zen_mode_screenshots_have_no_window_chrome() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (zen, normal) = (tmp.path().join("zen.png"), tmp.path().join("normal.png"));
        let state = tmp.path().join("state.json");
        run(
            &tmp.path().join("zen"),
            &format!(
                "new\ntype Zen text\ncommand view.zen_mode\nscreenshot {zen} 320 200\n\
                 dump-state {state}\n",
                zen = quoted(&zen),
                state = quoted(&state),
            ),
        )
        .unwrap();
        run(
            &tmp.path().join("normal"),
            &format!(
                "new\ntype Zen text\nscreenshot {normal} 320 200\n",
                normal = quoted(&normal)
            ),
        )
        .unwrap();
        let dump: Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(dump["windows"][0]["mode"], "Zen");
        let ((size, zen_colors), (_, normal_colors)) = (png_facts(&zen), png_facts(&normal));
        assert_eq!(size, (320, 200));
        assert!(
            zen_colors < normal_colors,
            "the window chrome is missing: {zen_colors} against {normal_colors} colors"
        );
    }

    #[test]
    fn a_screenshot_resolves_pictures_against_the_folder_of_the_document() {
        use image::codecs::png::PngEncoder;
        use image::{ExtendedColorType, ImageEncoder};

        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut red = Vec::new();
        PngEncoder::new(&mut red)
            .write_image(
                &[255, 0, 0, 255].repeat(40 * 40),
                40,
                40,
                ExtendedColorType::Rgba8,
            )
            .unwrap();
        let mut shots = Vec::new();
        for (name, with_picture) in [("with", true), ("without", false)] {
            let folder = tmp.path().join(name);
            std::fs::create_dir_all(&folder).unwrap();
            let notes = folder.join("notes.md");
            std::fs::write(&notes, "![](pic.png)\n").unwrap();
            if with_picture {
                std::fs::write(folder.join("pic.png"), &red).unwrap();
            }
            let shot = tmp.path().join(format!("{name}.png"));
            let script = format!(
                "open {}\nscreenshot {} 320 200\n",
                quoted(&notes),
                quoted(&shot)
            );
            run(&tmp.path().join(format!("profile-{name}")), &script).unwrap();
            shots.push(shot);
        }
        assert!(red_pixels(&shots[0]) > 500, "the picture is drawn");
        assert!(red_pixels(&shots[1]) < 100, "no picture, no red");
    }

    #[test]
    fn screenshot_steps_need_a_main_window_and_a_path_with_a_folder_part_to_write_to() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        // The empty path has no folder to create and cannot be written.
        let (line, message) = failure(run(tmp.path(), "new\nscreenshot \"\" 64 64\n"));
        assert_eq!(line, 2);
        assert!(message.contains("cannot write"), "{message}");
        // The same for the Settings window of a large bitmap.
        let (line, message) = failure(run(
            tmp.path(),
            "command settings.open\nscreenshot-settings x.png 8192 8192 4\n",
        ));
        assert_eq!(line, 2);
        assert!(message.contains("larger than"), "{message}");
        // The frames before a picture go to the main window.
        let mut runner = ran(tmp.path(), "command settings.open\n");
        let id = runner.controller().active_window().unwrap().id;
        runner.controller().close_window(id);
        for step in ["screenshot x.png 64 64", "screenshot-settings x.png 64 64"] {
            let (line, message) =
                failure(runner.run(&Script::parse(&format!("{step}\n")).unwrap()));
            assert_eq!(line, 1, "{step}");
            assert!(message.contains("has no window"), "{step}: {message}");
        }
        runner.finish();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_renderer_that_cannot_start_fails_the_steps_that_need_it() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let shot = Shot {
            path: tmp.path().join("never.png"),
            width: 64,
            height: 64,
            scale_percent: 100,
        };
        let steps = [
            Op::Theme(ThemeChoice::Dark),
            Op::Screenshot(shot.clone()),
            Op::ScreenshotSettings(shot.clone()),
        ];
        for (index, step) in steps.iter().enumerate() {
            let mut runner = ScriptRunner::start(core(&tmp.path().join(index.to_string())));
            // The first system call of the step is the start of the renderer.
            ssw_win::fault::arm(0);
            let outcome = runner.step(step);
            let reached = ssw_win::fault::tripped();
            ssw_win::fault::disarm();
            assert!(reached, "{step:?} started the renderer");
            let message = outcome.unwrap_err();
            assert!(
                message.starts_with("cannot start the screenshot renderer: "),
                "{step:?}: {message}"
            );
        }
        assert!(!shot.path.exists());
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_picture_the_renderer_cannot_draw_is_not_written() {
        let _com = ssw_win::com::ComGuard::apartment_threaded().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let visuals = Visuals::new().unwrap();
        let shot = Shot {
            path: tmp.path().join("never.png"),
            width: 64,
            height: 64,
            scale_percent: 100,
        };
        let geometry = Geometry::new(64, 64, 1.0).unwrap();
        // Every system call of a render fails in turn: the first is the creation of the Direct2D
        // factory, the last ones come after the picture was painted. Whichever it is, the
        // failure is the render's and no file is written (the run that gets through writes it).
        let mut painted = 0_u32;
        let outcomes = ssw_win::fault::sweep(|| {
            let outcome = write_shot(&visuals, &shot, geometry, Color::WHITE, &mut |_| {
                painted += 1;
            });
            (outcome, shot.path.exists())
        });
        assert!(outcomes.len() >= 8, "a render makes many system calls");
        for (call, (outcome, written)) in outcomes.iter().enumerate() {
            let message = outcome.as_ref().unwrap_err();
            assert!(
                message.starts_with("cannot render the screenshot: "),
                "call {call}: {message}"
            );
            assert!(!written, "call {call} wrote the picture");
        }
        assert!(
            painted > 0,
            "the later calls are reached after the painting"
        );
        assert!(shot.path.exists(), "the render that got through wrote it");
    }

    #[test]
    fn exiting_through_the_menu_command_ends_the_script() {
        let tmp = tempfile::tempdir().unwrap();
        run(
            tmp.path(),
            "new\ncommand file.exit\nassert-text-is never reached\n",
        )
        .unwrap();
    }

    #[test]
    fn a_session_file_that_vanished_fails_at_startup_with_the_unexpected_dialog() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("vanishing.md");
        std::fs::write(&file, "here\n").unwrap();
        let profile = tmp.path().join("profile");
        run(&profile, &format!("open {}\n", quoted(&file))).unwrap();
        std::fs::remove_file(&file).unwrap();
        let (line, message) = failure(run(&profile, "new\n"));
        assert_eq!(line, 0);
        assert!(
            message.starts_with("at startup: unexpected dialog"),
            "{message}"
        );
    }
}
