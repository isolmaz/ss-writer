//! Editor configuration, localized strings and the events the view reports to its owner.

use std::sync::Arc;

use ssw_core::edit::Selection;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::settings::{ListIndent, Settings};
use ssw_core::text::TextRange;

use crate::geometry::Rect;
use crate::input::ImeAttr;

/// Smallest zoom the view accepts, in percent.
pub const MIN_ZOOM_PERCENT: u32 = 50;
/// Largest zoom the view accepts, in percent.
pub const MAX_ZOOM_PERCENT: u32 = 400;
/// One `Ctrl+=` / `Ctrl+-` step, in percent.
pub const ZOOM_STEP_PERCENT: u32 = 10;

/// Everything the editor reads from the user's settings (spec §10 *Editor*) plus the app-wide
/// zoom. Build it with [`EditorConfig::from_settings`] and hand it to
/// [`EditorView::set_config`](super::EditorView::set_config).
#[derive(Debug, Clone, PartialEq)]
pub struct EditorConfig {
    /// Body font family.
    pub text_font: Arc<str>,
    /// Heading font family.
    pub heading_font: Arc<str>,
    /// Code font family.
    pub code_font: Arc<str>,
    /// Body font size in DIPs at 100 % zoom.
    pub font_size: f32,
    /// Line height as a multiple of the font size (used for estimates of blocks that are not
    /// laid out yet and for the height of blank lines).
    pub line_height: f32,
    /// Extra space between paragraphs, in em of the body font.
    pub paragraph_spacing: f32,
    /// Maximum width of the text column in DIPs at 100 % zoom.
    pub line_width: f32,
    /// Hide inline Markdown markers unless the caret touches the element.
    pub auto_hide_markdown: bool,
    /// Typewriter caret position, fraction of the viewport height.
    pub typewriter_position: f32,
    /// Typewriter dead zone, fraction of the viewport height on each side of the position.
    pub typewriter_dead_zone: f32,
    /// Indent unit of `Tab` in list items.
    pub list_indent: ListIndent,
    /// Move completed tasks to the end of their list.
    pub sort_completed_tasks: bool,
    /// Re-align table pipes when the caret leaves a table.
    pub format_tables_on_exit: bool,
    /// Zoom in percent (100 = 1:1).
    pub zoom_percent: u32,
}

impl EditorConfig {
    /// The configuration described by `settings` (editor section and zoom).
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        let editor = &settings.editor;
        Self {
            text_font: Arc::from(editor.text_font.as_str()),
            heading_font: Arc::from(editor.heading_font.as_str()),
            code_font: Arc::from(editor.code_font.as_str()),
            font_size: editor.font_size,
            line_height: editor.line_height,
            paragraph_spacing: editor.paragraph_spacing,
            line_width: editor.line_width as f32,
            auto_hide_markdown: editor.auto_hide_markdown,
            typewriter_position: editor.typewriter_position,
            typewriter_dead_zone: editor.typewriter_dead_zone,
            list_indent: editor.list_indent,
            sort_completed_tasks: editor.sort_completed_tasks,
            format_tables_on_exit: editor.format_tables_on_exit,
            zoom_percent: settings
                .appearance
                .zoom_percent
                .clamp(MIN_ZOOM_PERCENT, MAX_ZOOM_PERCENT),
        }
    }

    /// The zoom as a factor (`1.0` at 100 %).
    #[must_use]
    pub fn zoom(&self) -> f32 {
        self.zoom_percent as f32 / 100.0
    }

    /// Body font size with zoom applied.
    #[must_use]
    pub fn body_size(&self) -> f32 {
        self.font_size * self.zoom()
    }

    /// Height of one body line with zoom applied.
    #[must_use]
    pub fn line_px(&self) -> f32 {
        self.body_size() * self.line_height
    }

    /// Width of the text column with zoom applied.
    #[must_use]
    pub fn column_width(&self) -> f32 {
        self.line_width * self.zoom()
    }

    /// Space between paragraphs with zoom applied.
    #[must_use]
    pub fn spacing_px(&self) -> f32 {
        self.paragraph_spacing * self.body_size()
    }
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self::from_settings(&Settings::default())
    }
}

/// The localized strings the editor needs (message ids in `assets/i18n/<locale>/editor.ftl`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorStrings {
    /// Accessible name of the editor surface.
    pub name: String,
    /// Gutter badge of a heading with the caret (`H1` … `H6`), indexed by level − 1.
    pub heading_badges: [String; 6],
    /// Label of the pill shown after a folded heading.
    pub fold_pill: String,
    /// Accessible description of a heading (`Heading level 2`), indexed by level − 1.
    pub heading_names: [String; 6],
    /// Accessible description of a paragraph.
    pub paragraph: String,
    /// Accessible description of a blank line.
    pub blank_line: String,
    /// Accessible description of any other block.
    pub block: String,
}

/// The English source of the editor strings (`assets/i18n/en/editor.ftl`).
pub const EN_FTL: &str = include_str!("../../../../assets/i18n/en/editor.ftl");

impl Default for EditorStrings {
    /// The English strings.
    fn default() -> Self {
        Self::new(&Localizer::new("en", &[("en", EN_FTL)]))
    }
}

impl EditorStrings {
    /// Resolves every string through `localizer`.
    #[must_use]
    pub fn new(localizer: &Localizer) -> Self {
        let per_level = |id: &str| -> [String; 6] {
            std::array::from_fn(|index| {
                localizer.format(id, &[("level", Arg::Int(index as i64 + 1))])
            })
        };
        Self {
            name: localizer.text("editor-access-name"),
            heading_badges: per_level("editor-heading-badge"),
            fold_pill: localizer.text("editor-fold-pill"),
            heading_names: per_level("editor-access-heading"),
            paragraph: localizer.text("editor-access-paragraph"),
            blank_line: localizer.text("editor-access-blank-line"),
            block: localizer.text("editor-access-block"),
        }
    }
}

/// What a link in the text points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// An inline, reference or auto link: the resolved destination.
    Url(String),
    /// `[[target]]`, `[[target#heading]]`, `[[#heading]]`.
    Wiki {
        /// Document name (empty for `[[#heading]]`).
        target: String,
        /// Heading within the document.
        heading: Option<String>,
    },
    /// `[^label]`.
    Footnote(String),
}

/// An IME composition shown at the caret. Nothing is written to the document until it is
/// committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    /// Source offset the composition string is displayed at.
    pub at: usize,
    /// The composition string.
    pub text: String,
    /// Caret inside `text`, in UTF-8 bytes.
    pub caret: usize,
    /// Display attributes of segments of `text`.
    pub attrs: Vec<ImeAttr>,
}

/// Something the view wants its owner to do. Drain with
/// [`EditorView::take_events`](super::EditorView::take_events).
#[derive(Debug, Clone, PartialEq)]
pub enum EditorEvent {
    /// The user clicked a link with Ctrl held.
    LinkActivated(LinkTarget),
    /// The user clicked the fold chevron of the heading block that starts at `heading_start`.
    ToggleFold {
        /// Source offset of the heading block.
        heading_start: usize,
    },
    /// The user clicked the gutter badge of a heading: show the level menu.
    HeadingMenu {
        /// Source range of the heading block.
        range: TextRange,
        /// Current level.
        level: u8,
        /// The badge in window DIPs.
        anchor: Rect,
    },
    /// A zoom command or `Ctrl+wheel` changed the zoom; the app persists and broadcasts it.
    ZoomChanged(u32),
    /// Typewriter or focus mode was toggled.
    ModesChanged {
        /// Typewriter mode is on.
        typewriter: bool,
        /// Focus mode is on.
        focus: bool,
    },
    /// The selection changed (by keyboard, pointer or an edit).
    SelectionChanged(Selection),
    /// The user selected text with the pointer: the app may show the selection toolbar near
    /// `anchor` (window DIPs).
    SelectionToolbar {
        /// The selection's bounding box.
        anchor: Rect,
    },
    /// The text changed through the view.
    Edited,
}
