//! Inline objects: images and math laid out inside a line of text.
//!
//! The editor does not render math or decode images. For every `$x$` and `![alt](path)` it asks
//! an [`InlineObjectProvider`]; the app implements it with `ssw-render` (math scenes) and WIC
//! (images) and answers with the object's size, baseline and the platform handle to draw.

use crate::canvas::{ImageId, SceneId};
use crate::geometry::{Color, Size};
use ssw_core::text::TextRange;

/// What kind of object the editor asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    /// `$x$`: `source` is the TeX between the dollar signs.
    Math,
    /// `$$ … $$` block: `source` is the TeX between the delimiters, typeset in display style;
    /// `max_width` is the text column.
    DisplayMath,
    /// A Mermaid diagram (`mermaid` fence or `.mmd` document): `source` is the diagram source;
    /// the scene must fit `max_width`. [`ObjectState::Failed`] means the diagram is unsupported
    /// or failed to render.
    Mermaid,
    /// `![alt](dest)`, a reference image or `<img>`: `source` is the destination.
    Image,
}

/// A question to an [`InlineObjectProvider`].
#[derive(Debug, Clone, PartialEq)]
pub struct ObjectRequest<'a> {
    /// What the object is.
    pub kind: ObjectKind,
    /// TeX source (math) or destination (image), as resolved by the parser.
    pub source: &'a str,
    /// Alt text source (images) or empty.
    pub alt: &'a str,
    /// Requested width from `|300` / `width=` (images).
    pub width: Option<u32>,
    /// Requested height from `|300x200` / `height=` (images).
    pub height: Option<u32>,
    /// Font size of the surrounding text in DIPs (math scales with it).
    pub font_size: f32,
    /// Width of the text column in DIPs; images must not be wider.
    pub max_width: f32,
    /// Color of the surrounding text (math is drawn in it).
    pub text_color: Color,
    /// Where the element is in the document.
    pub range: TextRange,
}

/// What to draw for a ready object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectVisual {
    /// A vector scene (math, SVG).
    Scene(SceneId),
    /// A decoded raster image.
    Image(ImageId),
}

/// A ready object.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ObjectInfo {
    /// What to draw.
    pub visual: ObjectVisual,
    /// Size in DIPs.
    pub size: Size,
    /// Distance from the top of the object to the text baseline.
    pub baseline: f32,
}

/// The provider's answer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ObjectState {
    /// The object can be drawn.
    Ready(ObjectInfo),
    /// Still being produced (decoding, typesetting, downloading): the source text is shown muted
    /// until the app asks the view to relayout.
    Pending,
    /// A remote image that is not loaded until the user asks (`http`, or remote images turned
    /// off): shown like [`ObjectState::Pending`]; image blocks offer "Click to load", which
    /// raises a media event the app answers by loading the image and relayouting.
    Blocked,
    /// The object cannot be produced (math error, unsupported or missing image): the source is
    /// shown in the `error` color.
    Failed,
}

/// Supplies the objects of images and inline math. Called during layout, on the UI thread, so
/// implementations answer from a cache and start background work for misses.
pub trait InlineObjectProvider {
    /// The state of the object described by `request`.
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState;
}

/// A provider that has no objects: every element shows its source text as a pending object.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoObjects;

impl InlineObjectProvider for NoObjects {
    fn object(&self, _request: &ObjectRequest<'_>) -> ObjectState {
        ObjectState::Pending
    }
}
