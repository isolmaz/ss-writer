//! OS services the UI depends on, as traits.
//!
//! `ssw-win` provides the real implementations (OLE clipboard, `IFileDialog`, `ShellExecuteEx`,
//! `SetTimer`, `QueryPerformanceCounter`). Only [`SystemClock`], [`FakeClock`] and
//! [`MemoryClipboard`] live here: they are complete behaviors, not mocks of OS calls.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;

/// Failure of an OS service call.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ServiceError {
    /// The service cannot be used right now (clipboard locked by another process, no window).
    #[error("{service} is unavailable")]
    Unavailable { service: &'static str },
    /// The service call failed.
    #[error("{service} failed: {message}")]
    Failed {
        service: &'static str,
        message: String,
    },
}

/// Identifies a timer for [`Timers`] and `Event::Timer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(u64);

impl TimerId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// A process-unique id.
    #[must_use]
    pub fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Timers and animation-frame scheduling, driven by the host message loop.
pub trait Timers {
    /// Starts (or restarts) timer `id`; the host delivers `Event::Timer(id)` after `interval`
    /// and, when `repeat`, every `interval` after that.
    fn set(&mut self, id: TimerId, interval: Duration, repeat: bool);
    /// Stops timer `id`; unknown ids are ignored.
    fn kill(&mut self, id: TimerId);
    /// Asks for one `Event::AnimationFrame` on the next display refresh.
    fn request_animation_frame(&mut self);
}

/// A monotonic clock.
pub trait Clock {
    /// Time since an arbitrary fixed origin; never decreases.
    fn now(&self) -> Duration;
}

/// The real monotonic clock, measuring from its creation.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}

/// A clock that only moves when told to.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: Cell<Duration>,
}

impl FakeClock {
    /// Moves time forward by `by`.
    pub fn advance(&self, by: Duration) {
        self.now.set(self.now.get() + by);
    }

    /// Sets the absolute time.
    pub fn set(&self, to: Duration) {
        self.now.set(to);
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.now.get()
    }
}

/// A clipboard format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ClipboardFormat {
    /// `CF_UNICODETEXT`
    Text,
    /// `HTML Format`
    Html,
    /// `Rich Text Format`
    Rtf,
    /// A bitmap, exchanged as PNG bytes.
    Image,
    /// `CF_HDROP`
    Files,
    /// A registered format such as `vscode-editor-data`.
    Custom(String),
}

/// Everything placed on the clipboard by one copy. Absent fields are not offered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClipboardContent {
    pub text: Option<String>,
    pub html: Option<String>,
    pub rtf: Option<String>,
    /// PNG-encoded image.
    pub image: Option<Vec<u8>>,
    pub files: Vec<PathBuf>,
    /// Registered formats by name.
    pub custom: Vec<(String, Vec<u8>)>,
}

impl ClipboardContent {
    /// Content with only plain text.
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }
}

/// The system clipboard. Reads can fail when another process holds the clipboard open.
pub trait Clipboard {
    /// Replaces the clipboard with `content`, offering every present format at once.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be opened or written.
    fn set(&mut self, content: ClipboardContent) -> Result<(), ServiceError>;
    /// Empties the clipboard.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be opened.
    fn clear(&mut self) -> Result<(), ServiceError>;
    /// The formats currently available, in the order the source offered them.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be opened.
    fn formats(&self) -> Result<Vec<ClipboardFormat>, ServiceError>;
    /// Plain text, if offered.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    fn text(&self) -> Result<Option<String>, ServiceError>;
    /// The HTML fragment, if offered.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    fn html(&self) -> Result<Option<String>, ServiceError>;
    /// Rich text, if offered.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    fn rtf(&self) -> Result<Option<String>, ServiceError>;
    /// A PNG-encoded image, if offered.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    fn image(&self) -> Result<Option<Vec<u8>>, ServiceError>;
    /// Dropped/copied file paths; empty when none are offered.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    fn files(&self) -> Result<Vec<PathBuf>, ServiceError>;
    /// The bytes of the registered format `name`, if offered.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    fn custom(&self, name: &str) -> Result<Option<Vec<u8>>, ServiceError>;
}

/// A clipboard held in memory.
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    content: ClipboardContent,
}

impl Clipboard for MemoryClipboard {
    fn set(&mut self, content: ClipboardContent) -> Result<(), ServiceError> {
        self.content = content;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), ServiceError> {
        self.content = ClipboardContent::default();
        Ok(())
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, ServiceError> {
        let content = &self.content;
        let mut formats = Vec::new();
        if content.text.is_some() {
            formats.push(ClipboardFormat::Text);
        }
        if content.html.is_some() {
            formats.push(ClipboardFormat::Html);
        }
        if content.rtf.is_some() {
            formats.push(ClipboardFormat::Rtf);
        }
        if content.image.is_some() {
            formats.push(ClipboardFormat::Image);
        }
        if !content.files.is_empty() {
            formats.push(ClipboardFormat::Files);
        }
        formats.extend(
            content
                .custom
                .iter()
                .map(|(name, _)| ClipboardFormat::Custom(name.clone())),
        );
        Ok(formats)
    }

    fn text(&self) -> Result<Option<String>, ServiceError> {
        Ok(self.content.text.clone())
    }

    fn html(&self) -> Result<Option<String>, ServiceError> {
        Ok(self.content.html.clone())
    }

    fn rtf(&self) -> Result<Option<String>, ServiceError> {
        Ok(self.content.rtf.clone())
    }

    fn image(&self) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(self.content.image.clone())
    }

    fn files(&self) -> Result<Vec<PathBuf>, ServiceError> {
        Ok(self.content.files.clone())
    }

    fn custom(&self, name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        Ok(self
            .content
            .custom
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, bytes)| bytes.clone()))
    }
}

/// A file-type filter of a file dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFilter {
    /// Display name, e.g. "Markdown".
    pub name: String,
    /// Extensions without the dot, e.g. `["md", "markdown"]`.
    pub extensions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenFileOptions {
    pub title: String,
    pub filters: Vec<FileFilter>,
    pub allow_multiple: bool,
    pub initial_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SaveFileOptions {
    pub title: String,
    pub filters: Vec<FileFilter>,
    pub suggested_name: Option<String>,
    /// Extension appended when the user types none, without the dot.
    pub default_extension: Option<String>,
    pub initial_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PickFolderOptions {
    pub title: String,
    pub initial_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageBoxButtons {
    #[default]
    Ok,
    OkCancel,
    YesNo,
    YesNoCancel,
    /// Save / Don't save / Cancel.
    SaveDontSaveCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageBoxIcon {
    Information,
    Warning,
    Error,
    Question,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MessageBoxOptions {
    pub title: String,
    pub message: String,
    pub buttons: MessageBoxButtons,
    pub icon: Option<MessageBoxIcon>,
}

/// The button that closed a message box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageBoxResult {
    Ok,
    Cancel,
    Yes,
    No,
    Save,
    DontSave,
}

/// Modal system dialogs. `None` results mean the user cancelled.
pub trait Dialogs {
    /// Shows the open-file dialog.
    ///
    /// # Errors
    /// Fails when the dialog cannot be shown.
    fn open_file(
        &mut self,
        options: &OpenFileOptions,
    ) -> Result<Option<Vec<PathBuf>>, ServiceError>;
    /// Shows the save-file dialog.
    ///
    /// # Errors
    /// Fails when the dialog cannot be shown.
    fn save_file(&mut self, options: &SaveFileOptions) -> Result<Option<PathBuf>, ServiceError>;
    /// Shows the folder picker.
    ///
    /// # Errors
    /// Fails when the dialog cannot be shown.
    fn pick_folder(&mut self, options: &PickFolderOptions)
    -> Result<Option<PathBuf>, ServiceError>;
    /// Shows a message box and returns the pressed button.
    ///
    /// # Errors
    /// Fails when the message box cannot be shown.
    fn message_box(
        &mut self,
        options: &MessageBoxOptions,
    ) -> Result<MessageBoxResult, ServiceError>;
}

/// Shell integration.
pub trait Shell {
    /// Opens a URL in the default browser.
    ///
    /// # Errors
    /// Fails when the shell rejects the URL.
    fn open_url(&mut self, url: &str) -> Result<(), ServiceError>;
    /// Opens a file or folder with its default handler.
    ///
    /// # Errors
    /// Fails when the shell cannot open the path.
    fn open_path(&mut self, path: &Path) -> Result<(), ServiceError>;
    /// Shows the item selected in Explorer.
    ///
    /// # Errors
    /// Fails when Explorer cannot be launched.
    fn reveal_in_explorer(&mut self, path: &Path) -> Result<(), ServiceError>;
    /// Moves the item to the Recycle Bin.
    ///
    /// # Errors
    /// Fails when the item cannot be recycled.
    fn recycle(&mut self, path: &Path) -> Result<(), ServiceError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_ids_are_unique_and_round_trip() {
        let a = TimerId::allocate();
        let b = TimerId::allocate();
        assert_ne!(a, b);
        assert!(b > a);
        assert_eq!(TimerId::new(42).get(), 42);
    }

    #[test]
    fn fake_clock_only_moves_when_told() {
        let clock = FakeClock::default();
        assert_eq!(clock.now(), Duration::ZERO);
        clock.advance(Duration::from_millis(250));
        clock.advance(Duration::from_millis(250));
        assert_eq!(clock.now(), Duration::from_millis(500));
        clock.set(Duration::from_secs(10));
        assert_eq!(clock.now(), Duration::from_secs(10));
        let dynamic: &dyn Clock = &clock;
        assert_eq!(dynamic.now(), Duration::from_secs(10));
    }

    #[test]
    fn system_clock_is_monotonic() {
        let clock = SystemClock::new();
        let first = clock.now();
        let fresh = SystemClock::default().now();
        let third = clock.now();
        assert!(third >= first);
        // A new clock counts from its own creation, not from the process start.
        assert!(fresh < Duration::from_secs(60));
    }

    #[test]
    fn memory_clipboard_round_trips_every_format() {
        let mut clipboard = MemoryClipboard::default();
        assert_eq!(clipboard.formats(), Ok(vec![]));
        assert_eq!(clipboard.text(), Ok(None));
        assert_eq!(clipboard.html(), Ok(None));
        assert_eq!(clipboard.rtf(), Ok(None));
        assert_eq!(clipboard.image(), Ok(None));
        assert_eq!(clipboard.files(), Ok(vec![]));
        assert_eq!(clipboard.custom("vscode-editor-data"), Ok(None));

        let content = ClipboardContent {
            text: Some("**bold**".to_owned()),
            html: Some("<b>bold</b>".to_owned()),
            rtf: Some("{\\rtf1 \\b bold}".to_owned()),
            image: Some(vec![0x89, b'P', b'N', b'G']),
            files: vec![
                PathBuf::from("C:\\notes\\a.md"),
                PathBuf::from("C:\\notes\\b.png"),
            ],
            custom: vec![
                (
                    "vscode-editor-data".to_owned(),
                    b"{\"mode\":\"rust\"}".to_vec(),
                ),
                ("other".to_owned(), vec![1, 2, 3]),
            ],
        };
        assert_eq!(clipboard.set(content.clone()), Ok(()));
        assert_eq!(clipboard.text(), Ok(Some("**bold**".to_owned())));
        assert_eq!(clipboard.html(), Ok(Some("<b>bold</b>".to_owned())));
        assert_eq!(clipboard.rtf(), Ok(Some("{\\rtf1 \\b bold}".to_owned())));
        assert_eq!(clipboard.image(), Ok(Some(vec![0x89, b'P', b'N', b'G'])));
        assert_eq!(clipboard.files(), Ok(content.files.clone()));
        assert_eq!(clipboard.custom("other"), Ok(Some(vec![1, 2, 3])));
        assert_eq!(clipboard.custom("missing"), Ok(None));
        assert_eq!(
            clipboard.formats(),
            Ok(vec![
                ClipboardFormat::Text,
                ClipboardFormat::Html,
                ClipboardFormat::Rtf,
                ClipboardFormat::Image,
                ClipboardFormat::Files,
                ClipboardFormat::Custom("vscode-editor-data".to_owned()),
                ClipboardFormat::Custom("other".to_owned()),
            ])
        );

        // A new copy replaces everything, not just the formats it names.
        assert_eq!(clipboard.set(ClipboardContent::from_text("plain")), Ok(()));
        assert_eq!(clipboard.formats(), Ok(vec![ClipboardFormat::Text]));
        assert_eq!(clipboard.html(), Ok(None));
        assert_eq!(clipboard.clear(), Ok(()));
        assert_eq!(clipboard.formats(), Ok(vec![]));
        assert_eq!(clipboard.text(), Ok(None));
    }

    #[test]
    fn clipboard_content_helpers() {
        let plain = ClipboardContent::from_text(String::from("x"));
        assert_eq!(plain.text.as_deref(), Some("x"));
        assert!(plain.html.is_none() && plain.files.is_empty() && plain.custom.is_empty());
        assert_eq!(ClipboardContent::default().text, None);
    }

    #[test]
    fn service_errors_describe_the_failing_service() {
        let unavailable = ServiceError::Unavailable {
            service: "clipboard",
        };
        assert_eq!(unavailable.to_string(), "clipboard is unavailable");
        let failed = ServiceError::Failed {
            service: "shell",
            message: "access denied".to_owned(),
        };
        assert_eq!(failed.to_string(), "shell failed: access denied");
        assert_ne!(unavailable, failed);
    }

    #[test]
    fn dialog_options_default_to_empty_requests() {
        let open = OpenFileOptions::default();
        assert!(open.title.is_empty() && open.filters.is_empty() && !open.allow_multiple);
        let save = SaveFileOptions::default();
        assert!(save.suggested_name.is_none() && save.default_extension.is_none());
        let folder = PickFolderOptions::default();
        assert!(folder.initial_dir.is_none());
        let message = MessageBoxOptions::default();
        assert_eq!(message.buttons, MessageBoxButtons::Ok);
        assert_eq!(message.icon, None);
        let filter = FileFilter {
            name: "Markdown".to_owned(),
            extensions: vec!["md".to_owned()],
        };
        assert_eq!(filter.clone(), filter);
    }
}
