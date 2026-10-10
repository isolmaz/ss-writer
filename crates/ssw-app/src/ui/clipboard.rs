//! Copy, cut and paste of the editor (spec §2.3, §9).
//!
//! # Copy
//!
//! `Ctrl+C` / `Ctrl+X` put the selected Markdown on the clipboard as text, as `HTML Format` and
//! as `Rich Text Format` (the writers of `ssw-convert`, with the local images of the selection
//! embedded), plus the private [`FORMAT_MARKDOWN`] format that lets a later paste into SS Writer
//! take the exact source instead of converting the HTML back. Edit → Copy as offers each form
//! alone ([`CopyKind`]). Documents that are not Markdown copy their text only.
//!
//! # Paste
//!
//! [`plan_paste`] reads the clipboard once and applies the priority of spec §2.3:
//!
//! 1. SS Writer's own copy → the exact Markdown.
//! 2. A picture and nothing else → saved in the attachment folder, `![](relative/path.png)`.
//! 3. Files (`CF_HDROP`) → pictures embedded, other files linked.
//! 4. VS Code (`vscode-editor-data`) or HTML that is one monospace block → code.
//! 5. HTML → Markdown (`ssw_convert::html_to_md`); pictures embedded as `data:` URIs are saved
//!    like pasted pictures.
//! 6. Plain text (a URL over a selection becomes a link).
//!
//! `Ctrl+Shift+V`, a caret inside a code block and documents that are not Markdown always paste
//! the plain text. Code that comes from VS Code or a monospace block is inserted literally when
//! it is one line and as a fenced block when it has several lines and the caret is outside a
//! code block; code copied from a Markdown or plain-text file in VS Code is ordinary text.
//!
//! Pictures are written on the UI thread: a clipboard bitmap is already PNG-encoded, so the work
//! is one small file write, and the text can only be inserted once the file exists.
//!
//! # Attachments
//!
//! The attachment folder and the file naming come from the settings (spec §3.3, §10):
//! [`AttachmentTarget`] and [`save_numbered`]. A document without a file has no attachment
//! folder: pasting a picture into it tells the user to save first, and pasted files are linked
//! by their absolute path.

use std::cell::Cell;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_convert::assets::{AssetProvider, image_mime_for_path, sniff_image_mime};
use ssw_convert::html::render_fragment;
use ssw_convert::html_to_md::{HtmlPaste, convert_html_paste};
use ssw_convert::import::Asset;
use ssw_convert::rtf::markdown_to_rtf;
use ssw_convert::txt::markdown_to_text;
use ssw_core::command::CommandId;
use ssw_core::keymap::vk;
use ssw_core::links::{eq_ordinal_ignore_case, percent_decode};
use ssw_core::ops::format::{escape_brackets, link_destination};
use ssw_core::settings::{AttachmentLocation, Attachments};
use ssw_io::fileops::{NameStyle, free_name, validate_name};
use ssw_io::fs::{FileSystem, FsError};
use ssw_ui::editor::EditorView;
use ssw_ui::input::KeyEvent;
use ssw_ui::services::{
    Clipboard, ClipboardContent, ClipboardFormat, MessageBoxButtons, MessageBoxIcon,
    MessageBoxOptions, ServiceError,
};
use ssw_win::services::clipboard::{FORMAT_VSCODE_EDITOR_DATA, parse_vscode_mode};

use super::controller::{Controller, WindowCell};
use crate::documents::{DocEntry, DocError, DocMode};

/// The clipboard format every copy of SS Writer adds: the copied Markdown as UTF-8. Pasting into
/// SS Writer prefers it to the HTML, which would only be converted back (lossily).
pub const FORMAT_MARKDOWN: &str = "SS Writer Markdown";

/// The folder the HTML converter names pasted pictures under; replaced by their real places.
const ASSET_MARKER: &str = "ssw-paste-assets";

/// The naming template used when the settings hold one without a number.
const DEFAULT_TEMPLATE: &str = "image-{n}";

/// The largest local picture a copy embeds in its HTML and RTF.
pub const MAX_EMBEDDED_IMAGE: u64 = 8 * 1024 * 1024;

/// The most picture bytes one copy embeds.
pub const MAX_EMBEDDED_TOTAL: u64 = 32 * 1024 * 1024;

// ------------------------------------------------------------------------------------- copy

/// What a copy command puts on the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyKind {
    /// `Ctrl+C`, `Ctrl+X`: Markdown text, `HTML Format` and `Rich Text Format` (spec §9).
    Everything,
    /// Edit → Copy as → Plain text: the text without Markdown markers.
    PlainText,
    /// Edit → Copy as → Markdown: the source.
    Markdown,
    /// Edit → Copy as → Rich text: RTF and HTML, the plain text for targets without either.
    RichText,
    /// Edit → Copy as → HTML: the HTML fragment, also as text so a text editor receives the
    /// markup.
    Html,
}

/// The kind of copy `command` asks for; `None` for other commands.
#[must_use]
pub fn copy_kind(command: CommandId) -> Option<CopyKind> {
    match command {
        CommandId::EditCopy | CommandId::EditCut => Some(CopyKind::Everything),
        CommandId::EditCopyAsPlainText => Some(CopyKind::PlainText),
        CommandId::EditCopyAsMarkdown => Some(CopyKind::Markdown),
        CommandId::EditCopyAsRichText => Some(CopyKind::RichText),
        CommandId::EditCopyAsHtml => Some(CopyKind::Html),
        _ => None,
    }
}

/// The clipboard command a key press asks for while the editor has the focus: `Ctrl+C`,
/// `Ctrl+X`, `Ctrl+V` and `Ctrl+Shift+V`. Key releases, `Alt` and `Win` combinations give
/// `None`.
#[must_use]
pub fn key_command(key: &KeyEvent) -> Option<CommandId> {
    if !key.is_down || !key.mods.ctrl() || key.mods.alt() || key.mods.win() {
        return None;
    }
    match (key.vk, key.mods.shift()) {
        (vk::C, false) => Some(CommandId::EditCopy),
        (vk::X, false) => Some(CommandId::EditCut),
        (vk::V, false) => Some(CommandId::EditPaste),
        (vk::V, true) => Some(CommandId::EditPastePlain),
        _ => None,
    }
}

/// The clipboard content of copying `markdown` as `kind`; `assets` supplies the local images
/// the HTML and RTF embed.
#[must_use]
pub fn copy_content(
    kind: CopyKind,
    markdown: &str,
    assets: &dyn AssetProvider,
) -> ClipboardContent {
    match kind {
        CopyKind::Markdown => ClipboardContent::from_text(markdown),
        CopyKind::PlainText => ClipboardContent::from_text(plain_text(markdown)),
        CopyKind::Everything => ClipboardContent {
            text: Some(markdown.to_owned()),
            html: Some(render_fragment(markdown, assets)),
            rtf: Some(markdown_to_rtf(markdown, assets)),
            custom: vec![(FORMAT_MARKDOWN.to_owned(), markdown.as_bytes().to_vec())],
            ..ClipboardContent::default()
        },
        CopyKind::RichText => ClipboardContent {
            text: Some(plain_text(markdown)),
            html: Some(render_fragment(markdown, assets)),
            rtf: Some(markdown_to_rtf(markdown, assets)),
            ..ClipboardContent::default()
        },
        CopyKind::Html => {
            let html = render_fragment(markdown, assets);
            ClipboardContent {
                text: Some(html.clone()),
                html: Some(html),
                ..ClipboardContent::default()
            }
        }
    }
}

/// `markdown` as reader-visible text, without the line break `markdown_to_text` ends it with.
fn plain_text(markdown: &str) -> String {
    markdown_to_text(markdown).trim_end_matches('\n').to_owned()
}

/// The local images of copied Markdown, read relative to the document's folder. A copy of a
/// selection with pictures then carries them in its HTML and RTF; math and diagrams stay as
/// their source.
#[derive(Debug)]
pub struct LocalImages<'a> {
    fs: &'a dyn FileSystem,
    base: Option<&'a Path>,
    per_image: u64,
    remaining: Cell<u64>,
}

impl<'a> LocalImages<'a> {
    /// Images relative to `base` (`None` for a document without a file: nothing is found), read
    /// through `fs`, within [`MAX_EMBEDDED_IMAGE`] each and [`MAX_EMBEDDED_TOTAL`] together.
    #[must_use]
    pub fn new(fs: &'a dyn FileSystem, base: Option<&'a Path>) -> Self {
        Self::with_limits(fs, base, MAX_EMBEDDED_IMAGE, MAX_EMBEDDED_TOTAL)
    }

    /// [`LocalImages::new`] with other size limits (in bytes).
    #[must_use]
    pub fn with_limits(
        fs: &'a dyn FileSystem,
        base: Option<&'a Path>,
        per_image: u64,
        total: u64,
    ) -> Self {
        Self {
            fs,
            base,
            per_image,
            remaining: Cell::new(total),
        }
    }

    /// The bytes of `file` when it is a file within the limits.
    fn read(&self, file: &Path) -> Option<Vec<u8>> {
        let meta = self.fs.metadata(file).ok()?;
        if meta.is_dir() || meta.size > self.per_image || meta.size > self.remaining.get() {
            return None;
        }
        let bytes = self.fs.read(file).ok()?;
        self.remaining
            .set(self.remaining.get().saturating_sub(bytes.len() as u64));
        Some(bytes)
    }
}

impl AssetProvider for LocalImages<'_> {
    fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
        let base = self.base?;
        let decoded = percent_decode(path);
        // A destination is written raw (`<a b.png>`) by the editor and percent-encoded by
        // other tools.
        let bytes = self
            .read(&base.join(path))
            .or_else(|| self.read(&base.join(&decoded)))?;
        let mime = sniff_image_mime(&bytes).or_else(|| image_mime_for_path(&decoded))?;
        Some((bytes, mime))
    }

    fn math_svg(&self, _latex: &str, _display: bool) -> Option<String> {
        None
    }

    fn mermaid_svg(&self, _source: &str) -> Option<String> {
        None
    }
}

// ------------------------------------------------------------------------------------ paste

/// How the clipboard content is pasted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paste {
    /// Nothing the editor can use.
    Nothing,
    /// The plain text of the clipboard (a URL over a selection becomes a link).
    Text,
    /// Code from VS Code or a monospace block: literal, in a fenced block when it has several
    /// lines and the caret is not in a code block.
    Code(String),
    /// Markdown: SS Writer's own copy, or converted HTML with the pictures it carried.
    Markdown {
        /// The Markdown text; the pictures are referenced as `ssw-paste-assets/image-N.ext`.
        markdown: String,
        /// The pictures the HTML embedded.
        images: Vec<Asset>,
    },
    /// A picture (PNG bytes).
    Image(Vec<u8>),
    /// Files copied in Explorer.
    Files(Vec<PathBuf>),
}

/// Decides what the clipboard holds, by the priority of spec §2.3.
///
/// # Errors
/// The clipboard cannot be read.
pub fn plan_paste(clipboard: &dyn Clipboard) -> Result<Paste, ServiceError> {
    let formats = clipboard.formats()?;
    let has = |format: &ClipboardFormat| formats.contains(format);
    let own = ClipboardFormat::Custom(FORMAT_MARKDOWN.to_owned());
    if has(&own)
        && let Some(bytes) = clipboard.custom(FORMAT_MARKDOWN)?
    {
        return Ok(Paste::Markdown {
            markdown: String::from_utf8_lossy(&bytes).into_owned(),
            images: Vec::new(),
        });
    }
    let (text, html, files) = (
        has(&ClipboardFormat::Text),
        has(&ClipboardFormat::Html),
        has(&ClipboardFormat::Files),
    );
    if has(&ClipboardFormat::Image)
        && !(text || html || files)
        && let Some(png) = clipboard.image()?
    {
        return Ok(Paste::Image(png));
    }
    if files {
        let paths = clipboard.files()?;
        if !paths.is_empty() {
            return Ok(Paste::Files(paths));
        }
    }
    let vscode = has(&ClipboardFormat::Custom(
        FORMAT_VSCODE_EDITOR_DATA.to_owned(),
    ));
    if vscode && let Some(source) = clipboard.text()? {
        let mode = clipboard
            .custom(FORMAT_VSCODE_EDITOR_DATA)?
            .and_then(|data| parse_vscode_mode(&data));
        return Ok(
            if matches!(mode.as_deref(), Some("markdown" | "plaintext")) {
                Paste::Text
            } else {
                Paste::Code(source)
            },
        );
    }
    if html && let Some(fragment) = clipboard.html()? {
        match convert_html_paste(&fragment, vscode, ASSET_MARKER) {
            HtmlPaste::PlainText(code) => return Ok(Paste::Code(code)),
            HtmlPaste::Markdown(converted) if !converted.markdown.trim().is_empty() => {
                return Ok(Paste::Markdown {
                    markdown: converted.markdown.trim_end_matches('\n').to_owned(),
                    images: converted.assets,
                });
            }
            HtmlPaste::Markdown(_) => {}
        }
    }
    Ok(if text { Paste::Text } else { Paste::Nothing })
}

// ------------------------------------------------------------------------------ attachments

/// Where the attachments of one document live (spec §3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentTarget {
    doc_dir: PathBuf,
    folder: PathBuf,
}

impl AttachmentTarget {
    /// The attachment folder of the document file `document` under the attachment settings:
    /// next to the file, `assets/`, `<file>_assets/` or the custom folder (relative to the
    /// document's folder unless absolute; an empty custom folder means next to the file).
    #[must_use]
    pub fn new(document: &Path, settings: &Attachments) -> Self {
        let doc_dir = document.parent().map(Path::to_path_buf).unwrap_or_default();
        let folder = match settings.location {
            AttachmentLocation::NextToFile => doc_dir.clone(),
            AttachmentLocation::AssetsFolder => doc_dir.join("assets"),
            AttachmentLocation::FileAssetsFolder => {
                let stem = document
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                doc_dir.join(format!("{stem}_assets"))
            }
            AttachmentLocation::Custom => match settings.custom_folder.trim() {
                "" => doc_dir.clone(),
                custom => doc_dir.join(custom),
            },
        };
        Self { doc_dir, folder }
    }

    /// The folder pasted pictures are saved in.
    #[must_use]
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// The folder of the document; destinations are relative to it.
    #[must_use]
    pub fn doc_dir(&self) -> &Path {
        &self.doc_dir
    }

    /// The destination that points at `file` from the document: a relative path with `/`
    /// separators, or the absolute path when there is no relative one (another drive).
    #[must_use]
    pub fn destination(&self, file: &Path) -> String {
        relative_destination(&self.doc_dir, file).unwrap_or_else(|| to_slash(file))
    }

    /// Whether linking `file` in place keeps the document portable: it lies inside the
    /// document's folder or the workspace.
    fn reaches(&self, file: &Path, workspace: Option<&Path>) -> bool {
        [Some(self.doc_dir.as_path()), workspace]
            .into_iter()
            .flatten()
            .any(|root| {
                relative_destination(root, file)
                    .is_some_and(|relative| relative.split('/').next() != Some(".."))
            })
    }
}

/// `target` written relative to the folder `base` (`../` for every folder left, `/` between
/// segments, names compared without regard to case). `None` when the paths share no root
/// (different drives) or are the same.
#[must_use]
pub fn relative_destination(base: &Path, target: &Path) -> Option<String> {
    let base: Vec<Component<'_>> = base.components().collect();
    let target: Vec<Component<'_>> = target.components().collect();
    let same = |a: &Component<'_>, b: &Component<'_>| {
        eq_ordinal_ignore_case(
            &a.as_os_str().to_string_lossy(),
            &b.as_os_str().to_string_lossy(),
        )
    };
    if !same(base.first()?, target.first()?) {
        return None;
    }
    let common = base
        .iter()
        .zip(&target)
        .take_while(|(a, b)| same(a, b))
        .count();
    let mut parts = vec![String::from(".."); base.len() - common];
    parts.extend(
        target[common..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Saves `bytes` as the next free file of the attachment folder, named by `template` (`{n}` is
/// the running number: `image-1.png`, `image-2.png`, …; a template without `{n}` or a name
/// Windows rejects gives `image-{n}`). The folder is created when needed. The file is created
/// exclusively, so a name another program took in the meantime is skipped.
///
/// # Errors
/// Any failure of the file system other than a name that is taken.
pub fn save_numbered(
    fs: &dyn FileSystem,
    target: &AttachmentTarget,
    template: &str,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf, FsError> {
    fs.create_dir_all(target.folder())?;
    let template = if template.contains("{n}") {
        template
    } else {
        DEFAULT_TEMPLATE
    };
    let mut number = 1u32;
    loop {
        let mut name = format!(
            "{}.{extension}",
            template.replace("{n}", &number.to_string())
        );
        if validate_name(&name).is_err() {
            name = format!("image-{number}.{extension}");
        }
        let path = target.folder().join(name);
        match fs.metadata(&path) {
            Err(FsError::NotFound(_)) => match fs.write_new(&path, bytes) {
                Ok(()) => return Ok(path),
                Err(FsError::AlreadyExists(_)) => {}
                Err(error) => return Err(error),
            },
            Ok(_) => {}
            Err(error) => return Err(error),
        }
        number += 1;
    }
}

/// Copies `source` into `folder` under its own name (`name (2).ext` when that is taken). The
/// folder is created when needed.
///
/// # Errors
/// Any failure of the file system.
pub fn copy_to_folder(
    fs: &dyn FileSystem,
    folder: &Path,
    source: &Path,
) -> Result<PathBuf, FsError> {
    fs.create_dir_all(folder)?;
    let part = |name: Option<&std::ffi::OsStr>| {
        name.map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let name = free_name(
        fs,
        folder,
        &part(source.file_stem()),
        &part(source.extension()),
        NameStyle::Conflict,
    )?;
    let copy = folder.join(name);
    fs.copy_file(source, &copy)?;
    Ok(copy)
}

// ------------------------------------------------------------------------------ controller

impl Controller {
    /// Runs a copy, cut or paste command in the active editor of `cell`. `None` when `command`
    /// is none of them; otherwise whether it did something.
    pub(super) fn clipboard_command(
        &self,
        cell: &Rc<WindowCell>,
        command: CommandId,
    ) -> Option<bool> {
        if let Some(kind) = copy_kind(command) {
            return Some(self.copy(cell, kind, command == CommandId::EditCut));
        }
        match command {
            CommandId::EditPaste => Some(self.paste(cell, false)),
            CommandId::EditPastePlain => Some(self.paste(cell, true)),
            _ => None,
        }
    }

    fn copy(&self, cell: &Rc<WindowCell>, kind: CopyKind, cut: bool) -> bool {
        let selected = {
            let view = cell.view.borrow();
            view.active_tab().map(|tab| {
                let doc = tab.entry.document().borrow();
                let range = doc.primary_selection().range();
                (
                    Rc::clone(&tab.entry),
                    range,
                    doc.buffer().slice(range).into_owned(),
                )
            })
        };
        let Some((entry, range, source)) = selected else {
            return false;
        };
        if range.is_empty() {
            return false;
        }
        let content = if entry.mode() == DocMode::Markdown {
            let fs = Arc::clone(&self.docs().env().fs);
            let path = entry.path();
            let images = LocalImages::new(fs.as_ref(), path.as_deref().and_then(Path::parent));
            copy_content(kind, &source, &images)
        } else {
            ClipboardContent::from_text(source)
        };
        if self.clipboard.borrow_mut().set(content).is_err() {
            return false;
        }
        if cut {
            self.edit(cell, |editor| editor.remove_range(range));
        }
        true
    }

    fn paste(&self, cell: &Rc<WindowCell>, plain: bool) -> bool {
        let context = {
            let view = cell.view.borrow();
            view.active_tab().map(|tab| {
                let doc = tab.entry.document().borrow();
                let start = doc.primary_selection().range().start;
                (
                    Rc::clone(&tab.entry),
                    tab.workspace.clone(),
                    tab.editor.is_read_only(),
                    doc.ctx().in_verbatim_block(start),
                )
            })
        };
        let Some((entry, workspace, read_only, in_code)) = context else {
            return false;
        };
        if read_only {
            return true;
        }
        if plain || in_code || entry.mode() != DocMode::Markdown {
            return self.edit(cell, |editor| editor.execute(CommandId::EditPastePlain));
        }
        let planned = plan_paste(&*self.clipboard.borrow());
        match planned {
            Err(_) | Ok(Paste::Nothing) => false,
            Ok(Paste::Text) => self.edit(cell, |editor| editor.execute(CommandId::EditPaste)),
            Ok(Paste::Code(code)) => {
                let code = code.trim_end_matches('\n');
                self.edit(cell, |editor| {
                    if code.contains('\n') {
                        editor.paste_code(code);
                    } else {
                        editor.paste_markdown(code);
                    }
                    true
                })
            }
            Ok(Paste::Markdown { markdown, images }) => {
                self.paste_markdown(cell, &entry, markdown, &images)
            }
            Ok(Paste::Image(png)) => self.paste_image(cell, &entry, &png),
            Ok(Paste::Files(files)) => self.paste_files(cell, &entry, workspace.as_deref(), &files),
        }
    }

    /// Runs `action` on the active editor of `cell`.
    fn edit(&self, cell: &Rc<WindowCell>, action: impl FnOnce(&mut EditorView) -> bool) -> bool {
        cell.view
            .borrow_mut()
            .active_editor_mut()
            .is_some_and(action)
    }

    /// Pastes converted Markdown; the pictures it carried go to the attachment folder first.
    fn paste_markdown(
        &self,
        cell: &Rc<WindowCell>,
        entry: &DocEntry,
        mut markdown: String,
        images: &[Asset],
    ) -> bool {
        if !images.is_empty() {
            let pictures: Vec<(&str, &[u8])> = images
                .iter()
                .map(|image| {
                    let extension = Path::new(&image.relative_path)
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .unwrap_or("png");
                    (extension, image.bytes.as_slice())
                })
                .collect();
            let Some(destinations) = self.store_images(entry, &pictures) else {
                return true;
            };
            for (image, destination) in images.iter().zip(&destinations) {
                markdown = markdown.replace(
                    &format!("]({}", image.relative_path),
                    &format!("]({}", link_destination(destination)),
                );
            }
        }
        self.edit(cell, |editor| {
            editor.paste_markdown(&markdown);
            true
        })
    }

    /// Saves the clipboard picture and inserts `![](path)`.
    fn paste_image(&self, cell: &Rc<WindowCell>, entry: &DocEntry, png: &[u8]) -> bool {
        let Some(destination) = self
            .store_images(entry, &[("png", png)])
            .and_then(|mut destinations| destinations.pop())
        else {
            return true;
        };
        let image = format!("![]({})", link_destination(&destination));
        self.edit(cell, |editor| {
            editor.paste_markdown(&image);
            true
        })
    }

    /// Pastes copied files: pictures as images (copied into the attachment folder unless they
    /// already lie inside the document's folder or the workspace), other files as links to
    /// where they are.
    fn paste_files(
        &self,
        cell: &Rc<WindowCell>,
        entry: &DocEntry,
        workspace: Option<&Path>,
        files: &[PathBuf],
    ) -> bool {
        let fs = Arc::clone(&self.docs().env().fs);
        let target = entry
            .path()
            .map(|document| AttachmentTarget::new(&document, &self.settings().attachments));
        let mut copies: Vec<PathBuf> = Vec::new();
        let mut pieces = Vec::with_capacity(files.len());
        for file in files {
            let is_image = image_mime_for_path(&file.to_string_lossy()).is_some();
            let destination = match &target {
                None => to_slash(file),
                Some(target) if !is_image || target.reaches(file, workspace) => {
                    target.destination(file)
                }
                Some(target) => match copy_to_folder(fs.as_ref(), target.folder(), file) {
                    Ok(copy) => {
                        let destination = target.destination(&copy);
                        copies.push(copy);
                        destination
                    }
                    Err(error) => {
                        remove_files(fs.as_ref(), &copies);
                        self.report(&DocError::Fs(error));
                        return true;
                    }
                },
            };
            pieces.push(if is_image {
                format!("![]({})", link_destination(&destination))
            } else {
                let name = file
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                format!(
                    "[{}]({})",
                    escape_brackets(&name),
                    link_destination(&destination)
                )
            });
        }
        let markdown = pieces.join("\n\n");
        self.edit(cell, |editor| {
            editor.paste_markdown(&markdown);
            true
        })
    }

    /// Saves `pictures` (extension, bytes) in the attachment folder of `entry` and returns their
    /// destinations. A document without a file, or a failed write, is reported to the user and
    /// gives `None`; what was written before the failure is removed again.
    fn store_images(&self, entry: &DocEntry, pictures: &[(&str, &[u8])]) -> Option<Vec<String>> {
        let Some(document) = entry.path() else {
            let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
                title: self.i18n().text("window-error-title"),
                message: self.i18n().text("clipboard-save-first"),
                buttons: MessageBoxButtons::Ok,
                icon: Some(MessageBoxIcon::Information),
            });
            return None;
        };
        let (target, template) = {
            let settings = self.settings();
            (
                AttachmentTarget::new(&document, &settings.attachments),
                settings.attachments.naming_template.clone(),
            )
        };
        let fs = Arc::clone(&self.docs().env().fs);
        let mut saved: Vec<PathBuf> = Vec::with_capacity(pictures.len());
        for (extension, bytes) in pictures {
            match save_numbered(fs.as_ref(), &target, &template, extension, bytes) {
                Ok(path) => saved.push(path),
                Err(error) => {
                    remove_files(fs.as_ref(), &saved);
                    self.report(&DocError::Fs(error));
                    return None;
                }
            }
        }
        Some(saved.iter().map(|path| target.destination(path)).collect())
    }
}

/// Deletes files a failed paste had already written; a file that cannot be deleted stays.
fn remove_files(fs: &dyn FileSystem, files: &[PathBuf]) {
    for file in files {
        let _ = fs.delete_file(file);
    }
}
