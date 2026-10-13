//! Reading a file into text: plain text with encoding detection, the importers (DOCX, ODT, RTF,
//! DOC, TextBundle, TextPack) and the read-only check. Pure functions of a [`FileSystem`], so a
//! worker thread can run them and hand the [`Loaded`] result to the UI thread.

use std::path::{Path, PathBuf};

use ssw_convert::import::{Asset, ImportError, ImportResult, Loss};
use ssw_convert::textbundle::{self, TextBundleError};
use ssw_io::encoding::{self, TextEncoding, TextFormat};
use ssw_io::fs::{FileSystem, FsError};
use ssw_io::save::{SaveStamp, WriteAccess, write_access};
use ssw_io::workspace::{FileClass, classify_extension};

/// Word-processor formats opened by conversion to Markdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConvertedFormat {
    /// Office Open XML.
    Docx,
    /// OpenDocument text.
    Odt,
    /// Rich Text Format.
    Rtf,
    /// Word 97-2003 binary (import only).
    Doc,
}

impl ConvertedFormat {
    /// The format of a file extension (without the dot, any case).
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "docx" => Some(Self::Docx),
            "odt" => Some(Self::Odt),
            "rtf" => Some(Self::Rtf),
            "doc" => Some(Self::Doc),
            _ => None,
        }
    }

    /// The extension written for this format.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Odt => "odt",
            Self::Rtf => "rtf",
            Self::Doc => "doc",
        }
    }

    /// Whether "Save back" can write the format (`.doc` is import only).
    #[must_use]
    pub fn can_save_back(self) -> bool {
        self != Self::Doc
    }
}

/// Where a document's text came from and how it is written back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A text file: the bytes are the text in the document's [`TextFormat`].
    Text,
    /// Imported from a word-processor file; the file is only written by "Save back".
    Converted(ConvertedFormat),
    /// A `.textbundle` folder; `text_file` is the name of the text file inside it.
    TextBundle {
        /// Name of the text file inside the bundle.
        text_file: String,
    },
    /// A `.textpack` archive.
    TextPack,
}

/// How the editor treats a file (spec §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocMode {
    /// Markdown mode: live rendering.
    Markdown,
    /// Plain mode: no rendering.
    Plain,
    /// Mermaid source with a live diagram.
    Mermaid,
}

/// The editor mode for `path`, from its extension.
#[must_use]
pub fn mode_for_path(path: &Path) -> DocMode {
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "md" | "markdown" | "mdx" | "textbundle" | "textpack" | "docx" | "odt" | "rtf" | "doc" => {
            DocMode::Markdown
        }
        "mmd" => DocMode::Mermaid,
        _ => DocMode::Plain,
    }
}

/// The Fluent message id (`app.ftl`) naming a kind of content an importer could not represent.
#[must_use]
pub fn loss_message_id(kind: ssw_convert::import::LossKind) -> &'static str {
    use ssw_convert::import::LossKind as K;
    match kind {
        K::MergedCells => "app-loss-merged-cells",
        K::HeadersFooters => "app-loss-headers-footers",
        K::Columns => "app-loss-columns",
        K::TextBoxes => "app-loss-text-boxes",
        K::Comments => "app-loss-comments",
        K::TrackedChanges => "app-loss-tracked-changes",
        K::VectorShapes => "app-loss-vector-shapes",
        K::UnsupportedEquations => "app-loss-unsupported-equations",
        K::UnsupportedImages => "app-loss-unsupported-images",
        K::Alignment => "app-loss-alignment",
        K::Underline => "app-loss-underline",
        K::FontsAndColors => "app-loss-fonts-and-colors",
        K::SubSuperscript => "app-loss-sub-superscript",
        K::NestedTables => "app-loss-nested-tables",
        K::MultiParagraphCells => "app-loss-multi-paragraph-cells",
        K::Fields => "app-loss-fields",
        K::HiddenText => "app-loss-hidden-text",
        K::SymbolFontCharacters => "app-loss-symbol-font-characters",
        K::ListNumberingStyle => "app-loss-list-numbering-style",
        K::SectionBreaks => "app-loss-section-breaks",
        K::CorruptParts => "app-loss-corrupt-parts",
    }
}

/// A file read and decoded.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// Document text with `\n` line endings.
    pub text: String,
    /// Encoding, BOM and line endings to restore on save.
    pub format: TextFormat,
    /// Where the text came from.
    pub source: Source,
    /// What the importer could not represent (sorted by kind); empty for text files.
    pub losses: Vec<Loss>,
    /// Files extracted by the importer (relative to the Markdown file), or the assets of a
    /// TextPack.
    pub assets: Vec<Asset>,
    /// Identity of the file content as read (size, mtime, hash).
    pub stamp: SaveStamp,
    /// Whether the file can be written.
    pub access: WriteAccess,
    /// Malformed input was replaced: saving would not reproduce the original bytes.
    pub lossy: bool,
}

/// Why a file could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The file could not be read.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// An importer refused the file.
    #[error(transparent)]
    Import(#[from] ImportError),
    /// A TextBundle or TextPack is unreadable.
    #[error(transparent)]
    Bundle(#[from] TextBundleError),
    /// A file of no known text type whose content looks binary.
    #[error("{} is not a text file", .0.display())]
    NotText(PathBuf),
}

/// Names tried, in order, for the text file of a `.textbundle` folder.
const BUNDLE_TEXT_NAMES: [&str; 6] = [
    "text.markdown",
    "text.md",
    "text.mdown",
    "text.mkdn",
    "text.mkd",
    "text.txt",
];

fn extension_of(path: &Path) -> String {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Whether files with `extension` (lowercase, no dot) open as text without a look at their
/// content: Markdown and the known text formats. A file of any other type, or without one, is
/// text only when [`encoding::looks_binary`] says it is not binary.
fn is_known_text(extension: &str) -> bool {
    matches!(
        classify_extension(extension),
        FileClass::Markdown | FileClass::Text
    )
}

/// The extensions of the TextBundle formats, which have their own reader.
const BUNDLE_EXTENSIONS: [&str; 2] = ["textbundle", "textpack"];

/// Whether files with `extension` are read by a converter, which decides itself what it accepts
/// (a DOCX or TextPack is a ZIP, a DOC a compound file: binary by content, text once imported).
/// The word-processor formats are those of [`ConvertedFormat`], so a converter added there is
/// never sniffed as binary and refused before its importer runs.
fn is_converted(extension: &str) -> bool {
    ConvertedFormat::from_extension(extension).is_some() || BUNDLE_EXTENSIONS.contains(&extension)
}

fn stem_of(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn imported(
    result: ImportResult,
    format: ConvertedFormat,
    stamp: SaveStamp,
    access: WriteAccess,
) -> Loaded {
    Loaded {
        losses: result.sorted_losses(),
        text: result.markdown,
        format: TextFormat::default(),
        source: Source::Converted(format),
        assets: result.assets,
        stamp,
        access,
        lossy: false,
    }
}

/// Imports a `.doc` file. Files that are not OLE containers are routed by their content, as
/// the importer's error says what they look like.
fn import_doc(
    bytes: &[u8],
    assets_dir: &str,
) -> Result<(ImportResult, ConvertedFormat), ImportError> {
    match ssw_convert::doc::import(bytes, assets_dir) {
        Ok(result) => Ok((result, ConvertedFormat::Doc)),
        Err(ImportError::WrongFormat { detected, .. }) => match detected {
            "RTF" => {
                ssw_convert::rtf::import_rtf(bytes).map(|result| (result, ConvertedFormat::Rtf))
            }
            "DOCX" => ssw_convert::docx::import_docx(bytes, assets_dir)
                .map(|result| (result, ConvertedFormat::Docx)),
            "HTML" => Ok((
                ssw_convert::html_to_md::html_to_markdown(
                    &String::from_utf8_lossy(bytes),
                    assets_dir,
                ),
                ConvertedFormat::Doc,
            )),
            "text" => Ok((
                ImportResult {
                    markdown: encoding::decode(bytes, None).text,
                    ..ImportResult::default()
                },
                ConvertedFormat::Doc,
            )),
            other => Err(ImportError::WrongFormat {
                expected: "Word",
                detected: other,
            }),
        },
        Err(error) => Err(error),
    }
}

/// The text file of the `.textbundle` folder at `dir`.
fn bundle_text_file(fs: &dyn FileSystem, dir: &Path) -> Result<String, FsError> {
    let names: Vec<String> = fs
        .list_dir(dir)?
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    for wanted in BUNDLE_TEXT_NAMES {
        if let Some(name) = names.iter().find(|name| name.eq_ignore_ascii_case(wanted)) {
            return Ok(name.clone());
        }
    }
    Ok(names
        .into_iter()
        .filter(|name| name.len() > 5 && name.as_bytes()[..5].eq_ignore_ascii_case(b"text."))
        .min()
        .unwrap_or_else(|| BUNDLE_TEXT_NAMES[0].to_owned()))
}

/// Reads and decodes `path`: text files with encoding detection (`tld` biases the legacy code
/// page guess), word-processor files through their importer, bundles and packs through the
/// TextBundle reader. A file whose extension is no known Markdown or text type (or that has
/// none) is text when its content does not look binary, see [`encoding::looks_binary`].
///
/// # Errors
/// [`LoadError`] when the file cannot be read, is refused by an importer, is a broken
/// bundle, or is a binary file of no known text type ([`LoadError::NotText`]).
pub fn load_file(fs: &dyn FileSystem, path: &Path, tld: Option<&str>) -> Result<Loaded, LoadError> {
    load_file_as(fs, path, tld, None)
}

/// [`load_file`] where `forced` names the encoding a text file is decoded with instead of the
/// detected one ("reopen with encoding", spec §4.2). Files read through an importer have no
/// encoding to choose and ignore it.
///
/// # Errors
/// As [`load_file`].
pub fn load_file_as(
    fs: &dyn FileSystem,
    path: &Path,
    tld: Option<&str>,
    forced: Option<TextEncoding>,
) -> Result<Loaded, LoadError> {
    let meta = fs.metadata(path)?;
    let access = write_access(fs, path).unwrap_or(WriteAccess::Writable);
    let extension = extension_of(path);
    if meta.is_dir() {
        let bundle = textbundle::read_dir(path)?;
        let text_file = bundle_text_file(fs, path)?;
        return Ok(Loaded {
            stamp: SaveStamp::from_read(&meta, bundle.text.as_bytes()),
            text: bundle.text,
            format: TextFormat::default(),
            source: Source::TextBundle { text_file },
            losses: Vec::new(),
            assets: Vec::new(),
            access,
            lossy: false,
        });
    }
    if !is_known_text(&extension) && !is_converted(&extension) {
        // A file of unknown type is classified by its first bytes, so a picture, an archive or
        // a disk image is refused without reading it whole.
        let head = fs.read_prefix(path, encoding::BINARY_SNIFF_LEN)?;
        if encoding::looks_binary(&head) {
            return Err(LoadError::NotText(path.to_path_buf()));
        }
    }
    let bytes = fs.read(path)?;
    ssw_win::perf::mark_latest(crate::perf::DOC_READ);
    let stamp = SaveStamp::from_read(&meta, &bytes);
    let stem = stem_of(path);
    let assets_dir = format!("{stem}_assets");
    Ok(match extension.as_str() {
        "docx" => imported(
            ssw_convert::docx::import_docx(&bytes, &stem)?,
            ConvertedFormat::Docx,
            stamp,
            access,
        ),
        "odt" => imported(
            ssw_convert::odt::import_odt(&bytes, &stem)?,
            ConvertedFormat::Odt,
            stamp,
            access,
        ),
        "rtf" => imported(
            ssw_convert::rtf::import_rtf(&bytes)?,
            ConvertedFormat::Rtf,
            stamp,
            access,
        ),
        "doc" => {
            let (result, format) = import_doc(&bytes, &assets_dir)?;
            imported(result, format, stamp, access)
        }
        "textbundle" | "textpack" => {
            let bundle = textbundle::read_pack(&bytes)?;
            Loaded {
                text: bundle.text,
                format: TextFormat::default(),
                source: Source::TextPack,
                losses: Vec::new(),
                assets: bundle.assets,
                stamp,
                access,
                lossy: false,
            }
        }
        _ => {
            let decoded = match forced {
                Some(chosen) => encoding::decode_as(&bytes, chosen),
                None => encoding::decode(&bytes, tld),
            };
            ssw_win::perf::mark_latest(crate::perf::DOC_DECODED);
            Loaded {
                text: decoded.text,
                format: decoded.format,
                source: Source::Text,
                losses: Vec::new(),
                assets: Vec::new(),
                stamp,
                access,
                lossy: decoded.lossy,
            }
        }
    })
}

/// The path with the Win32 extended-length prefix removed (`\\?\C:\x` → `C:\x`,
/// `\\?\UNC\server\share` → `\\server\share`), so display and watcher paths agree.
#[must_use]
pub fn strip_extended_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        let drive_letter = rest.len() >= 2
            && rest.as_bytes()[0].is_ascii_alphabetic()
            && rest.as_bytes()[1] == b':';
        if drive_letter {
            PathBuf::from(rest)
        } else {
            path.to_path_buf()
        }
    } else {
        path.to_path_buf()
    }
}

/// The canonical form of `path` used as the registry identity: symlinks and `..` resolved, the
/// extended prefix removed. A file that does not exist yet keeps its name inside the canonical
/// parent folder.
#[must_use]
pub fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return strip_extended_prefix(&canonical);
    }
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match (absolute.parent(), absolute.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| strip_extended_prefix(&parent).join(name))
            .unwrap_or(absolute),
        _ => absolute,
    }
}

#[cfg(test)]
mod tests {
    use ssw_convert::import::LossKind;
    use ssw_io::fs::{FaultFs, FsFailure, FsOp, RealFs};

    use super::*;

    fn fixture(relative: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("tests")
            .join("fixtures")
            .join(relative)
    }

    /// A failed load as text: which stage refused and what it said.
    fn describe(error: &LoadError) -> String {
        match error {
            LoadError::Fs(error) => format!("file system: {error:?}"),
            LoadError::Import(error) => format!("import: {error:?}"),
            LoadError::Bundle(error) => format!("bundle: {error:?}"),
            LoadError::NotText(path) => format!("not text: {}", path.display()),
        }
    }

    #[test]
    fn extensions_map_to_converted_formats_and_modes() {
        assert_eq!(
            ConvertedFormat::from_extension("DOCX"),
            Some(ConvertedFormat::Docx)
        );
        assert_eq!(
            ConvertedFormat::from_extension("odt"),
            Some(ConvertedFormat::Odt)
        );
        assert_eq!(
            ConvertedFormat::from_extension("rtf"),
            Some(ConvertedFormat::Rtf)
        );
        assert_eq!(
            ConvertedFormat::from_extension("doc"),
            Some(ConvertedFormat::Doc)
        );
        assert_eq!(ConvertedFormat::from_extension("md"), None);
        for format in [
            ConvertedFormat::Docx,
            ConvertedFormat::Odt,
            ConvertedFormat::Rtf,
            ConvertedFormat::Doc,
        ] {
            assert_eq!(
                ConvertedFormat::from_extension(format.extension()),
                Some(format)
            );
            assert_eq!(format.can_save_back(), format != ConvertedFormat::Doc);
        }
        assert_eq!(mode_for_path(Path::new("a.MD")), DocMode::Markdown);
        assert_eq!(mode_for_path(Path::new("a.textbundle")), DocMode::Markdown);
        assert_eq!(mode_for_path(Path::new("a.docx")), DocMode::Markdown);
        assert_eq!(mode_for_path(Path::new("a.mmd")), DocMode::Mermaid);
        assert_eq!(mode_for_path(Path::new("a.txt")), DocMode::Plain);
        assert_eq!(mode_for_path(Path::new("Makefile")), DocMode::Plain);
    }

    #[test]
    fn a_text_file_is_decoded_with_its_format_and_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("note.md");
        std::fs::write(&path, b"\xEF\xBB\xBFone\r\ntwo\r\n").unwrap();
        let loaded = load_file(&RealFs, &path, None).unwrap();
        assert_eq!(loaded.text, "one\ntwo\n");
        assert!(loaded.format.bom);
        assert_eq!(loaded.format.line_ending, encoding::LineEnding::Crlf);
        assert_eq!(loaded.source, Source::Text);
        assert_eq!(loaded.access, WriteAccess::Writable);
        assert!(!loaded.lossy && loaded.losses.is_empty() && loaded.assets.is_empty());
        // 3 BOM bytes + "one\r\ntwo\r\n" (10 bytes).
        assert_eq!(loaded.stamp.size, 13);
        assert!(loaded.stamp.matches_content(&std::fs::read(&path).unwrap()));
    }

    #[test]
    fn a_file_of_no_known_text_type_is_text_unless_it_looks_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let path = tmp.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let text = [
            ("notes.kt", &b"fun main() {}\r\n"[..], "fun main() {}\n"),
            ("Dockerfile", b"FROM scratch\n", "FROM scratch\n"),
            (".gitignore", b"target/\n", "target/\n"),
            ("LICENSE", b"MIT\n", "MIT\n"),
            ("wide.dat", b"\xFF\xFEh\0i\0\n\0", "hi\n"),
            // Wide text without a BOM is text, as it is under a known text type.
            ("wide-without-bom.dat", b"h\0i\0\n\0", "hi\n"),
            ("wide-without-bom", b"\0h\0i\0\n", "hi\n"),
            ("wide32.dat", b"h\0\0\0i\0\0\0\n\0\0\0", "hi\n"),
        ];
        for (name, bytes, expected) in text {
            let loaded = load_file(&RealFs, &write(name, bytes), None).unwrap();
            assert_eq!(loaded.text, expected, "{name}");
            assert_eq!(loaded.source, Source::Text, "{name}");
        }
        let binary = [
            ("pic.png", &b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"[..]),
            ("tool.exe", b"MZ\x90\0\x03\0\0\0"),
            ("blob", b"\x7fELF\x02\x01\x01\0\0\0\0\0"),
            // The zero-filled start of a disk image is no UTF-32 text.
            ("disk.img", &[0; 8192]),
        ];
        for (name, bytes) in binary {
            let path = write(name, bytes);
            let error = load_file(&RealFs, &path, None).unwrap_err();
            assert_eq!(describe(&error), format!("not text: {}", path.display()));
            assert_eq!(
                error.to_string(),
                format!("{} is not a text file", path.display())
            );
        }
        // A known text type is opened without a look at its content: the same bytes as the
        // file without a BOM above.
        let wide = write("wide.txt", b"h\0i\0\n\0");
        assert_eq!(load_file(&RealFs, &wide, None).unwrap().text, "hi\n");
    }

    #[test]
    fn a_converter_extension_is_never_sniffed_for_binary() {
        let tmp = tempfile::tempdir().unwrap();
        // Bytes with NULs that no importer takes: the importer answers, not the sniff.
        let junk = b"\x00\x01\x02\x03junk\x00";
        let mut extensions: Vec<&str> = [
            ConvertedFormat::Docx,
            ConvertedFormat::Odt,
            ConvertedFormat::Rtf,
            ConvertedFormat::Doc,
        ]
        .into_iter()
        .map(ConvertedFormat::extension)
        .collect();
        extensions.extend(BUNDLE_EXTENSIONS);
        for extension in extensions {
            assert!(is_converted(extension), "{extension}");
            let path = tmp.path().join(format!("junk.{extension}"));
            std::fs::write(&path, junk).unwrap();
            let outcome = load_file(&RealFs, &path, None);
            assert!(
                !matches!(outcome, Err(LoadError::NotText(_))),
                "{extension}: refused as binary"
            );
        }
        assert!(!is_converted("md"));
        assert!(!is_converted("dat"));
    }

    #[test]
    fn a_missing_file_is_a_not_found_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.md");
        let error = load_file(&RealFs, &missing, None).unwrap_err();
        assert_eq!(
            describe(&error),
            format!("file system: {:?}", FsError::NotFound(missing))
        );
    }

    #[test]
    fn a_docx_fixture_imports_to_markdown_with_a_loss_report() {
        let path = fixture("docx/poi_heading123.docx");
        let loaded = load_file(&RealFs, &path, None).unwrap();
        assert_eq!(loaded.source, Source::Converted(ConvertedFormat::Docx));
        assert!(
            loaded.text.starts_with("# First paragraph\n\n"),
            "{}",
            loaded.text
        );
        assert_eq!(loaded.format, TextFormat::default());

        let commented = load_file(&RealFs, &fixture("docx/mammoth_comments.docx"), None).unwrap();
        assert!(
            commented
                .losses
                .iter()
                .any(|loss| loss.kind == LossKind::Comments && loss.count > 0),
            "{:?}",
            commented.losses
        );
        // Losses are sorted by kind.
        let kinds: Vec<LossKind> = commented.losses.iter().map(|loss| loss.kind).collect();
        let mut sorted = kinds.clone();
        sorted.sort();
        assert_eq!(kinds, sorted);
    }

    #[test]
    fn extracted_pictures_become_assets_named_after_the_file() {
        let loaded = load_file(&RealFs, &fixture("docx/mammoth_tiny-picture.docx"), None).unwrap();
        assert_eq!(loaded.assets.len(), 1);
        assert_eq!(
            loaded.assets[0].relative_path,
            "mammoth_tiny-picture_assets/image-1.png"
        );
    }

    #[test]
    fn an_encrypted_docx_is_refused() {
        let error = load_file(
            &RealFs,
            &fixture("docx/tika_testWORD_protected_passtika.docx"),
            None,
        )
        .unwrap_err();
        assert_eq!(describe(&error), "import: Encrypted");
    }

    #[test]
    fn an_odt_and_an_rtf_fixture_import() {
        let odt = load_file(&RealFs, &fixture("odt/LibreOfficeWriter_odt_1.3.odt"), None).unwrap();
        assert_eq!(odt.source, Source::Converted(ConvertedFormat::Odt));
        assert!(!odt.text.trim().is_empty());
        let rtf = load_file(&RealFs, &fixture("rtf/testRTFBoldItalic.rtf"), None).unwrap();
        assert_eq!(rtf.source, Source::Converted(ConvertedFormat::Rtf));
        assert!(!rtf.text.trim().is_empty());
    }

    #[test]
    fn a_doc_fixture_imports_through_the_ole_reader() {
        let loaded = load_file(&RealFs, &fixture("doc/simple-list.doc"), None).unwrap();
        assert_eq!(loaded.source, Source::Converted(ConvertedFormat::Doc));
        assert!(!loaded.text.trim().is_empty());
        assert!(loaded.losses.is_empty(), "{:?}", loaded.losses);
        let encrypted = load_file(&RealFs, &fixture("doc/PasswordProtected.doc"), None);
        assert_eq!(describe(&encrypted.unwrap_err()), "import: Encrypted");
    }

    #[test]
    fn a_doc_extension_with_rtf_content_is_routed_to_the_rtf_importer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("really-rtf.doc");
        std::fs::write(&path, br"{\rtf1\ansi Hello \b bold\b0 world.\par}").unwrap();
        let loaded = load_file(&RealFs, &path, None).unwrap();
        assert_eq!(loaded.source, Source::Converted(ConvertedFormat::Rtf));
        assert!(loaded.text.contains("Hello"), "{}", loaded.text);
    }

    #[test]
    fn a_doc_extension_with_html_or_text_content_is_routed_by_content() {
        let tmp = tempfile::tempdir().unwrap();
        let html = tmp.path().join("page.doc");
        std::fs::write(
            &html,
            b"<html><body><h1>Title</h1><p>Body text</p></body></html>",
        )
        .unwrap();
        let loaded = load_file(&RealFs, &html, None).unwrap();
        assert!(loaded.text.contains("Title"), "{}", loaded.text);
        let text = tmp.path().join("plain.doc");
        std::fs::write(&text, b"just some words\nand another line\n").unwrap();
        let loaded = load_file(&RealFs, &text, None).unwrap();
        assert!(loaded.text.contains("just some words"));
    }

    #[test]
    fn a_doc_extension_with_unknown_binary_content_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pdf.doc");
        std::fs::write(&path, b"%PDF-1.4\n%binary").unwrap();
        let error = load_file(&RealFs, &path, None).unwrap_err();
        let expected = ImportError::WrongFormat {
            expected: "Word",
            detected: "PDF",
        };
        assert_eq!(describe(&error), format!("import: {expected:?}"));
        assert_eq!(error.to_string(), expected.to_string());
    }

    #[test]
    fn a_broken_office_file_is_an_import_error() {
        let tmp = tempfile::tempdir().unwrap();
        let garbage: &[u8] = b"this is not a zip archive";
        let expected = [
            ("docx", ssw_convert::docx::import_docx(garbage, "broken")),
            ("odt", ssw_convert::odt::import_odt(garbage, "broken")),
            ("rtf", ssw_convert::rtf::import_rtf(garbage)),
        ];
        for (extension, imported) in expected {
            let path = tmp.path().join(format!("broken.{extension}"));
            std::fs::write(&path, garbage).unwrap();
            let error = load_file(&RealFs, &path, None).unwrap_err();
            assert_eq!(
                describe(&error),
                format!("import: {:?}", imported.unwrap_err()),
                "{extension}"
            );
        }
    }

    #[test]
    fn a_textbundle_folder_and_a_textpack_are_read() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("note.textbundle");
        let asset = Asset {
            relative_path: "assets/pic.png".to_owned(),
            bytes: vec![1, 2, 3],
        };
        textbundle::write_dir(&bundle, "# Bundle\n", std::slice::from_ref(&asset)).unwrap();
        let loaded = load_file(&RealFs, &bundle, None).unwrap();
        assert_eq!(loaded.text, "# Bundle\n");
        assert_eq!(
            loaded.source,
            Source::TextBundle {
                text_file: "text.markdown".to_owned()
            }
        );

        let pack = tmp.path().join("note.textpack");
        std::fs::write(
            &pack,
            textbundle::write_pack("# Pack\n", std::slice::from_ref(&asset)).unwrap(),
        )
        .unwrap();
        let loaded = load_file(&RealFs, &pack, None).unwrap();
        assert_eq!(loaded.text, "# Pack\n");
        assert_eq!(loaded.source, Source::TextPack);
        assert_eq!(loaded.assets, [asset]);

        std::fs::write(tmp.path().join("bad.textpack"), b"nope").unwrap();
        let error = load_file(&RealFs, &tmp.path().join("bad.textpack"), None).unwrap_err();
        assert_eq!(
            describe(&error),
            format!("bundle: {:?}", textbundle::read_pack(b"nope").unwrap_err())
        );
    }

    #[test]
    fn a_doc_extension_with_docx_content_is_routed_to_the_docx_importer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("really-docx.doc");
        std::fs::copy(fixture("docx/poi_heading123.docx"), &path).unwrap();
        let loaded = load_file(&RealFs, &path, None).unwrap();
        assert_eq!(loaded.source, Source::Converted(ConvertedFormat::Docx));
        assert!(
            loaded.text.starts_with("# First paragraph"),
            "{}",
            loaded.text
        );
    }

    #[test]
    fn a_folder_without_a_text_file_is_no_bundle_and_a_missing_one_cannot_be_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let empty = tmp.path().join("empty.textbundle");
        std::fs::create_dir(&empty).unwrap();
        let error = load_file(&RealFs, &empty, None).unwrap_err();
        assert_eq!(error.to_string(), "the bundle has no text file");
        let missing = bundle_text_file(&RealFs, &tmp.path().join("missing")).unwrap_err();
        assert_eq!(
            std::mem::discriminant(&missing),
            std::mem::discriminant(&FsError::NotFound(PathBuf::new()))
        );
    }

    #[test]
    fn the_bundle_text_file_name_follows_the_reader_order() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("b.textbundle");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("text.mkd"), "x").unwrap();
        std::fs::write(dir.join("text.md"), "y").unwrap();
        assert_eq!(bundle_text_file(&RealFs, &dir).unwrap(), "text.md");
        std::fs::remove_file(dir.join("text.md")).unwrap();
        std::fs::remove_file(dir.join("text.mkd")).unwrap();
        std::fs::write(dir.join("text.org"), "z").unwrap();
        assert_eq!(bundle_text_file(&RealFs, &dir).unwrap(), "text.org");
        std::fs::remove_file(dir.join("text.org")).unwrap();
        assert_eq!(bundle_text_file(&RealFs, &dir).unwrap(), "text.markdown");
    }

    #[test]
    fn extended_prefixes_are_stripped_only_where_it_is_safe() {
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\C:\a\b.md")),
            Path::new(r"C:\a\b.md")
        );
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\UNC\srv\share\a.md")),
            Path::new(r"\\srv\share\a.md")
        );
        assert_eq!(
            strip_extended_prefix(Path::new(r"\\?\Volume{1234}\a.md")),
            Path::new(r"\\?\Volume{1234}\a.md")
        );
        assert_eq!(
            strip_extended_prefix(Path::new(r"C:\a.md")),
            Path::new(r"C:\a.md")
        );
    }

    #[test]
    fn canonical_paths_resolve_dots_and_keep_new_file_names() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "x").unwrap();
        let through_dots = tmp.path().join("sub").join("..").join("a.md");
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let canonical = canonical_path(&through_dots);
        assert_eq!(canonical, canonical_path(&file));
        assert!(!canonical.to_string_lossy().starts_with(r"\\?\"));
        let fresh = canonical_path(&tmp.path().join("new.md"));
        assert_eq!(fresh.file_name().unwrap(), "new.md");
        assert_eq!(fresh.parent().unwrap(), canonical.parent().unwrap());
        // A path whose parent does not exist stays as given (made absolute).
        let orphan = canonical_path(&tmp.path().join("missing").join("x.md"));
        assert!(orphan.ends_with(Path::new("missing").join("x.md")));
        assert!(orphan.is_absolute());
    }

    #[test]
    fn a_path_that_cannot_be_made_absolute_or_has_no_name_stays_as_given() {
        // The empty path has no absolute form and neither a parent nor a file name.
        assert_eq!(canonical_path(Path::new("")), PathBuf::new());
    }

    #[test]
    fn a_forced_encoding_replaces_the_detected_one() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("latin.txt");
        std::fs::write(&path, b"caf\xE9\n").unwrap();
        let windows_1252 = TextEncoding::from_label("windows-1252").unwrap();
        let forced = load_file_as(&RealFs, &path, None, Some(windows_1252)).unwrap();
        assert_eq!(forced.text, "caf\u{E9}\n");
        assert_eq!(forced.format.encoding, windows_1252);
        assert!(!forced.lossy);
        // As UTF-8 the byte is malformed: it is replaced and the file is not reproducible.
        let utf8 = load_file_as(&RealFs, &path, None, Some(TextEncoding::Utf8)).unwrap();
        assert_eq!(utf8.text, "caf\u{FFFD}\n");
        assert_eq!(utf8.format.encoding, TextEncoding::Utf8);
        assert!(utf8.lossy);
    }

    #[test]
    fn a_file_system_that_fails_after_the_metadata_call_fails_the_load() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "x").unwrap();
        let fault = FaultFs::new();
        fault.fail(FsOp::Read, FsFailure::SharingViolation, 1);
        let error = load_file(&fault, &file, None).unwrap_err();
        assert_eq!(
            describe(&error),
            format!("file system: {:?}", FsError::SharingViolation(file.clone()))
        );
        // The failure was scripted once: the file reads now.
        assert_eq!(load_file(&fault, &file, None).unwrap().text, "x");

        let bundle = tmp.path().join("b.textbundle");
        textbundle::write_dir(&bundle, "# B\n", &[]).unwrap();
        fault.fail(FsOp::ListDir, FsFailure::AccessDenied, 1);
        let error = load_file(&fault, &bundle, None).unwrap_err();
        assert_eq!(
            describe(&error),
            format!("file system: {:?}", FsError::AccessDenied(bundle.clone()))
        );
        assert_eq!(load_file(&fault, &bundle, None).unwrap().text, "# B\n");
    }

    #[test]
    fn an_unknown_type_whose_first_bytes_cannot_be_read_fails_the_load_with_that_error() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("blob.dat");
        std::fs::write(&file, "plain words\n").unwrap();
        // The prefix read of the sniff is the first read of the load, and it fails.
        let fault = FaultFs::new();
        fault.fail(FsOp::Read, FsFailure::AccessDenied, 1);
        let error = load_file(&fault, &file, None).unwrap_err();
        assert_eq!(
            describe(&error),
            format!("file system: {:?}", FsError::AccessDenied(file.clone()))
        );
        assert_eq!(fault.calls(FsOp::Read), 1, "the whole file was not read");
        // The failure was scripted once: the sniff passes and the text loads.
        assert_eq!(
            load_file(&fault, &file, None).unwrap().text,
            "plain words\n"
        );
    }
}
