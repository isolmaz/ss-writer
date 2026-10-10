//! Renderers of code blocks (with syntax highlighting), math blocks, Mermaid diagrams, images
//! and attachments.
//!
//! [`register`] installs them with English strings and no attachment data;
//! [`register_with`] takes [`MediaServices`]. Lone images and attachment links are *special*
//! renderers of paragraphs (and of HTML blocks holding a single `<img>`), so they work whatever
//! other renderers are registered and in whatever order.

use std::rc::Rc;

use ssw_core::i18n::{Arg, Localizer};
use ssw_core::text::TextRange;

use crate::canvas::ImageId;
use crate::geometry::Rect;

use super::DocumentKind;
use super::block::{BlockClass, BlockRegistry};

mod attachment;
mod code;
mod diagram;
mod image;
mod verbatim;

/// The English source of the media strings (`assets/i18n/en/editor-media.ftl`).
pub const EN_FTL: &str = include_str!("../../../../../assets/i18n/en/editor-media.ftl");

/// Something the user did with a code, image or diagram control, reported as
/// [`EditorEvent::Media`](super::EditorEvent::Media).
#[derive(Debug, Clone, PartialEq)]
pub enum MediaEvent {
    /// The user pressed the Copy button of a code block: put `text` on the clipboard.
    CopyCode {
        /// Source range of the code block.
        range: TextRange,
        /// The code (every line followed by `\n`, as `CodeBlock::text`).
        text: String,
    },
    /// The user pressed "Click to load" on a remote image the provider reported as blocked:
    /// allow `dest` and relayout.
    LoadImage {
        /// The image destination.
        dest: String,
        /// Source range of the image.
        range: TextRange,
    },
    /// The user right-clicked a Mermaid diagram: show a menu with Export PNG and Export SVG for
    /// `source` (labels: [`MediaStrings::mermaid_export_png`] / `mermaid_export_svg`).
    MermaidMenu {
        /// Source range of the Mermaid block.
        range: TextRange,
        /// The diagram source.
        source: String,
        /// The click position as a zero-size rectangle in window DIPs.
        anchor: Rect,
    },
}

/// The localized strings of the media renderers (ids `media-…`).
#[derive(Debug)]
pub struct MediaStrings {
    localizer: Rc<Localizer>,
    /// Label of the Copy button of a code block.
    pub code_copy: String,
    /// Accessible name of a code block.
    pub code_block: String,
    /// Accessible name of a math block.
    pub math_block: String,
    /// Accessible name of a Mermaid diagram.
    pub mermaid_diagram: String,
    /// Note under the source of a diagram that cannot be drawn.
    pub mermaid_unsupported: String,
    /// Menu item: export the diagram as PNG.
    pub mermaid_export_png: String,
    /// Menu item: export the diagram as SVG.
    pub mermaid_export_svg: String,
    /// Placeholder of an image that is being loaded.
    pub image_loading: String,
    /// Placeholder of a blocked remote image.
    pub image_blocked: String,
    /// Placeholder of an image that failed to load.
    pub image_failed: String,
}

impl MediaStrings {
    /// Resolves every string through `localizer`.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>) -> Self {
        Self {
            code_copy: localizer.text("media-code-copy"),
            code_block: localizer.text("media-code-block"),
            math_block: localizer.text("media-math-block"),
            mermaid_diagram: localizer.text("media-mermaid-diagram"),
            mermaid_unsupported: localizer.text("media-mermaid-unsupported"),
            mermaid_export_png: localizer.text("media-mermaid-export-png"),
            mermaid_export_svg: localizer.text("media-mermaid-export-svg"),
            image_loading: localizer.text("media-image-loading"),
            image_blocked: localizer.text("media-image-blocked"),
            image_failed: localizer.text("media-image-failed"),
            localizer,
        }
    }

    /// Accessible name of a code block written in `language`.
    #[must_use]
    pub fn code_named(&self, language: &str) -> String {
        self.localizer.format(
            "media-code-block-named",
            &[("language", Arg::Str(language))],
        )
    }

    /// Accessible name of an image with alt text `alt` (may be empty).
    #[must_use]
    pub fn image_named(&self, alt: &str) -> String {
        if alt.is_empty() {
            self.localizer.text("media-image")
        } else {
            self.localizer
                .format("media-image-named", &[("alt", Arg::Str(alt))])
        }
    }

    /// Accessible name of the attachment `name`.
    #[must_use]
    pub fn attachment_named(&self, name: &str) -> String {
        self.localizer
            .format("media-attachment-named", &[("name", Arg::Str(name))])
    }
}

impl Default for MediaStrings {
    /// The English strings.
    fn default() -> Self {
        Self::new(Rc::new(Localizer::new("en", &[("en", EN_FTL)])))
    }
}

/// What the attachment chip shows about a local file.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentInfo {
    /// File name with extension.
    pub name: String,
    /// The TYPE pill (`PDF`, `ZIP`, …).
    pub type_label: String,
    /// `size · date`, formatted for the active locale by the app.
    pub detail: String,
    /// The shell icon of the file type, when the platform has loaded it.
    pub icon: Option<ImageId>,
}

/// Supplies the data of attachment chips: the app resolves the link destination against the
/// document folder, checks that it is a local file that is not an image, and reads its name,
/// type, size and date through the shell.
pub trait AttachmentProvider {
    /// The chip data for the link destination `dest`, or `None` when it is not a local
    /// non-image file (the link is then an ordinary link).
    fn attachment(&self, dest: &str) -> Option<AttachmentInfo>;
}

/// A provider that knows no files: every link stays an ordinary link.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAttachments;

impl AttachmentProvider for NoAttachments {
    fn attachment(&self, _dest: &str) -> Option<AttachmentInfo> {
        None
    }
}

/// What the media renderers need besides the document.
#[derive(Debug)]
pub struct MediaServices {
    /// Localized strings.
    pub strings: MediaStrings,
    /// Attachment chip data.
    pub attachments: Rc<dyn AttachmentProvider>,
}

impl std::fmt::Debug for dyn AttachmentProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttachmentProvider")
    }
}

impl Default for MediaServices {
    fn default() -> Self {
        Self {
            strings: MediaStrings::default(),
            attachments: Rc::new(NoAttachments),
        }
    }
}

/// Registers the media renderers with English strings and no attachment data.
pub fn register(registry: &mut BlockRegistry) {
    register_with(registry, MediaServices::default());
}

/// Registers the media renderers: code blocks (and `mermaid` fences), math blocks, and the
/// special renderers for lone images and attachment links.
pub fn register_with(registry: &mut BlockRegistry, services: MediaServices) {
    let strings = Rc::new(services.strings);
    let code = Rc::new(code::CodeRenderer::new(Rc::clone(&strings)));
    registry.register(BlockClass::CodeBlock, code.clone());
    registry.register_document(
        DocumentKind::Mermaid,
        Rc::new(diagram::MermaidDocumentRenderer::new(code)),
    );
    registry.register(
        BlockClass::MathBlock,
        Rc::new(diagram::MathRenderer::new(Rc::clone(&strings))),
    );
    let attachments = Rc::new(attachment::AttachmentRenderer::new(
        Rc::clone(&strings),
        services.attachments,
    ));
    registry.register_special(BlockClass::Paragraph, attachments);
    let images = Rc::new(image::ImageRenderer::new(strings));
    registry.register_special(BlockClass::Paragraph, images.clone());
    registry.register_special(BlockClass::HtmlBlock, images);
}
