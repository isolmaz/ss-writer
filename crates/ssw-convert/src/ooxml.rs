//! Shared OOXML plumbing: ZIP package reader with zip-bomb limits, a small namespace-aware XML
//! tree, relationship parsing/building, content types and XML escaping for the writers.
//!
//! The reader side ([`Package`], [`parse_xml`], [`Relationships`], [`sniff_container`]) is used by
//! the DOCX importer; the writer side ([`PackageWriter`], [`ContentTypes`],
//! [`RelationshipsBuilder`], [`escape_xml`], [`image_info`]) by the DOCX exporter. Other
//! OPC-style formats (ODT is not OPC, but shares the ZIP and XML code) may reuse the generic
//! pieces.

use std::collections::HashMap;
use std::io::{Cursor, Read, Write};

use quick_xml::NsReader;
use quick_xml::escape::{resolve_predefined_entity, unescape};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{NamespaceResolver, ResolveResult};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// English Metric Units per CSS pixel (96 dpi).
pub const EMU_PER_PX: u64 = 9525;

/// Maximum total uncompressed size of all parts of a package.
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 200 * 1024 * 1024;
/// Maximum number of entries in a package.
pub const MAX_ENTRIES: usize = 10_000;
/// Maximum element nesting depth accepted by [`parse_xml`].
pub const MAX_XML_DEPTH: usize = 128;

/// XML declaration written at the top of every generated part.
pub const XML_DECL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#;

/// Namespace URIs (transitional); strict variants are recognised by [`Ns::from_uri`].
pub mod uri {
    pub const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    pub const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    pub const WP: &str = "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing";
    pub const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
    pub const PIC: &str = "http://schemas.openxmlformats.org/drawingml/2006/picture";
    pub const M: &str = "http://schemas.openxmlformats.org/officeDocument/2006/math";
    pub const PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
    pub const CONTENT_TYPES: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
}

/// Relationship type URIs (transitional) used by the writer.
pub mod rel {
    pub const OFFICE_DOCUMENT: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument";
    pub const CORE_PROPERTIES: &str =
        "http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties";
    pub const EXTENDED_PROPERTIES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/extended-properties";
    pub const STYLES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles";
    pub const NUMBERING: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering";
    pub const SETTINGS: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/settings";
    pub const FOOTNOTES: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes";
    pub const HYPERLINK: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";
    pub const IMAGE: &str =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/image";
}

/// Why a package or XML part could not be read or written.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum PackageError {
    #[error("not a valid ZIP package: {0}")]
    Corrupt(String),
    #[error("package exceeds the size limit: {0}")]
    TooLarge(String),
    #[error("package I/O failure: {0}")]
    Io(String),
    #[error(transparent)]
    Xml(#[from] XmlError),
}

/// Why an XML part could not be parsed.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum XmlError {
    #[error("malformed XML: {0}")]
    Malformed(String),
    #[error("XML nesting is deeper than {MAX_XML_DEPTH} levels")]
    TooDeep,
    #[error("XML document has no root element")]
    Empty,
}

/// Namespace of an element, reduced to the ones the importers care about. Transitional and
/// ISO-strict URIs map to the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ns {
    /// WordprocessingML (`w:`).
    W,
    /// Office Math (`m:`).
    M,
    /// DrawingML (`a:`).
    A,
    /// WordprocessingML drawing (`wp:`).
    Wp,
    /// DrawingML picture (`pic:`).
    Pic,
    /// Relationship attributes (`r:`).
    R,
    /// VML (`v:`).
    V,
    /// Legacy Office attributes used by VML (`o:`).
    O,
    /// Markup compatibility (`mc:`).
    Mc,
    /// Word 2010 shapes (`wps:`).
    Wps,
    /// Package relationships (`Relationships` part).
    PkgRel,
    /// Any other bound namespace.
    Other,
    /// No namespace.
    None,
}

impl Ns {
    /// Maps a namespace URI (transitional or strict) to its [`Ns`].
    pub fn from_uri(uri: &str) -> Ns {
        match uri {
            "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
            | "http://purl.oclc.org/ooxml/wordprocessingml/main" => Ns::W,
            "http://schemas.openxmlformats.org/officeDocument/2006/math"
            | "http://purl.oclc.org/ooxml/officeDocument/math" => Ns::M,
            "http://schemas.openxmlformats.org/drawingml/2006/main"
            | "http://purl.oclc.org/ooxml/drawingml/main" => Ns::A,
            "http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"
            | "http://purl.oclc.org/ooxml/drawingml/wordprocessingDrawing" => Ns::Wp,
            "http://schemas.openxmlformats.org/drawingml/2006/picture"
            | "http://purl.oclc.org/ooxml/drawingml/picture" => Ns::Pic,
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
            | "http://purl.oclc.org/ooxml/officeDocument/relationships" => Ns::R,
            "urn:schemas-microsoft-com:vml" => Ns::V,
            "urn:schemas-microsoft-com:office:office" => Ns::O,
            "http://schemas.openxmlformats.org/markup-compatibility/2006" => Ns::Mc,
            "http://schemas.microsoft.com/office/word/2010/wordprocessingShape" => Ns::Wps,
            "http://schemas.openxmlformats.org/package/2006/relationships" => Ns::PkgRel,
            _ => Ns::Other,
        }
    }

    /// Namespace of an element or attribute name as resolved by quick-xml: unprefixed names
    /// without a default namespace are [`Ns::None`], names with an undeclared prefix [`Ns::Other`].
    fn of(resolved: &ResolveResult<'_>) -> Ns {
        match resolved {
            ResolveResult::Unbound => Ns::None,
            ResolveResult::Bound(ns) => Ns::from_uri(ns.0),
            ResolveResult::Unknown(_) => Ns::Other,
        }
    }
}

/// A child of an [`Element`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
}

/// An attribute with its namespace resolved (`Ns::None` when unprefixed). `xmlns` declarations
/// are not attributes of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub ns: Ns,
    /// Local name (prefix dropped).
    pub name: String,
    /// Unescaped value.
    pub value: String,
}

/// An XML element with its namespace resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub ns: Ns,
    pub name: String,
    pub attrs: Vec<Attribute>,
    pub children: Vec<Node>,
}

impl Element {
    /// True when the element has the given namespace and local name.
    pub fn is(&self, ns: Ns, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    /// Value of the first attribute with local name `name`, whatever its namespace.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.value.as_str())
    }

    /// Value of the attribute with local name `name` in namespace `ns` (`Ns::None` for
    /// unprefixed attributes).
    pub fn attr_ns(&self, ns: Ns, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| a.ns == ns && a.name == name)
            .map(|a| a.value.as_str())
    }

    /// Every element below this one (any depth, document order) with the given namespace and
    /// local name.
    pub fn descendants(&self, ns: Ns, name: &str) -> Vec<&Element> {
        let mut out = Vec::new();
        self.collect_descendants(ns, name, &mut out);
        out
    }

    fn collect_descendants<'e>(&'e self, ns: Ns, name: &str, out: &mut Vec<&'e Element>) {
        for child in self.elements() {
            if child.is(ns, name) {
                out.push(child);
            }
            child.collect_descendants(ns, name, out);
        }
    }

    /// Child elements in document order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// First child element with the given namespace and name.
    pub fn child(&self, ns: Ns, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.is(ns, name))
    }

    /// All text below this element, concatenated in document order.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.collect_text(&mut out);
        out
    }

    fn collect_text(&self, out: &mut String) {
        for node in &self.children {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Element(e) => e.collect_text(out),
            }
        }
    }
}

fn start_to_element(ns: Ns, start: &BytesStart<'_>, resolver: &NamespaceResolver) -> Element {
    let name = start.local_name().into_inner().to_string();
    let mut attrs = Vec::new();
    for attr in start.attributes().flatten() {
        if attr.key.as_namespace_binding().is_some() {
            continue;
        }
        let (resolved, local) = resolver.resolve_attribute(attr.key);
        let value = unescape(&attr.value)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| attr.value.to_string());
        attrs.push(Attribute {
            ns: Ns::of(&resolved),
            name: local.into_inner().to_string(),
            value,
        });
    }
    Element {
        ns,
        name,
        attrs,
        children: Vec::new(),
    }
}

fn push_text(stack: &mut [Element], text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(top) = stack.last_mut() {
        if let Some(Node::Text(last)) = top.children.last_mut() {
            last.push_str(text);
        } else {
            top.children.push(Node::Text(text.to_string()));
        }
    }
}

/// Parses an XML part into a namespace-resolved tree. A leading UTF-8 BOM is accepted. Nesting
/// deeper than [`MAX_XML_DEPTH`] is rejected so recursive consumers cannot overflow the stack.
pub fn parse_xml(bytes: &[u8]) -> Result<Element, XmlError> {
    parse_tree(bytes, false).map(|(root, _)| root)
}

/// Like [`parse_xml`] but keeps what precedes a syntax error or a premature end of the input
/// (truncated part): open elements are closed at that point. Returns the tree and whether the
/// input was damaged; `None` when nothing usable (no root element, or nesting too deep).
pub fn parse_xml_salvage(bytes: &[u8]) -> Option<(Element, bool)> {
    parse_tree(bytes, true).ok()
}

fn parse_tree(bytes: &[u8], lenient: bool) -> Result<(Element, bool), XmlError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    let mut reader = NsReader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    let mut damaged = false;
    loop {
        buf.clear();
        let (ns, event) = match reader.read_resolved_event_into(&mut buf) {
            Ok((resolved, event)) => (Ns::of(&resolved), event),
            Err(_) if lenient => {
                damaged = true;
                break;
            }
            Err(e) => return Err(XmlError::Malformed(e.to_string())),
        };
        let step = match event {
            Event::Start(start) => {
                if stack.len() >= MAX_XML_DEPTH {
                    return Err(XmlError::TooDeep);
                }
                stack.push(start_to_element(ns, &start, reader.resolver()));
                Ok(())
            }
            Event::Empty(start) => {
                let element = start_to_element(ns, &start, reader.resolver());
                attach(&mut stack, &mut root, element)
            }
            Event::End(_) => match stack.pop() {
                Some(element) => attach(&mut stack, &mut root, element),
                None => Err(XmlError::Malformed("unbalanced end tag".into())),
            },
            Event::Text(text) => {
                push_text(&mut stack, &text.xml10_content());
                Ok(())
            }
            Event::CData(data) => {
                push_text(&mut stack, &data.xml10_content());
                Ok(())
            }
            Event::GeneralRef(reference) => {
                match reference.resolve_char_ref() {
                    Ok(Some(c)) => push_text(&mut stack, c.encode_utf8(&mut [0u8; 4])),
                    Ok(None) => {
                        if let Some(s) = resolve_predefined_entity(&reference.xml10_content()) {
                            push_text(&mut stack, s);
                        }
                    }
                    Err(_) => {}
                }
                Ok(())
            }
            Event::Eof => break,
            _ => Ok(()),
        };
        if let Err(e) = step {
            if !lenient {
                return Err(e);
            }
            damaged = true;
            break;
        }
    }
    if !stack.is_empty() {
        if !lenient {
            return Err(XmlError::Malformed("unclosed element".into()));
        }
        damaged = true;
        while let Some(element) = stack.pop() {
            if attach(&mut stack, &mut root, element).is_err() {
                break;
            }
        }
    }
    root.map(|root| (root, damaged)).ok_or(XmlError::Empty)
}

fn attach(
    stack: &mut [Element],
    root: &mut Option<Element>,
    element: Element,
) -> Result<(), XmlError> {
    match stack.last_mut() {
        Some(parent) => parent.children.push(Node::Element(element)),
        None => {
            if root.is_some() {
                return Err(XmlError::Malformed("multiple root elements".into()));
            }
            *root = Some(element);
        }
    }
    Ok(())
}

fn normalize_name(name: &str) -> String {
    name.trim_start_matches('/')
        .replace('\\', "/")
        .to_lowercase()
}

/// A read-only ZIP package. Part lookup is case-insensitive; the total number of decompressed
/// bytes handed out is bounded by [`MAX_TOTAL_UNCOMPRESSED`].
#[derive(Debug)]
pub struct Package<'a> {
    archive: ZipArchive<Cursor<&'a [u8]>>,
    index: HashMap<String, usize>,
    names: Vec<String>,
    read_total: u64,
}

impl<'a> Package<'a> {
    /// Opens `bytes` as a ZIP archive and checks the entry-count and declared-size limits.
    pub fn open(bytes: &'a [u8]) -> Result<Package<'a>, PackageError> {
        let mut archive = ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| PackageError::Corrupt(e.to_string()))?;
        if archive.len() > MAX_ENTRIES {
            return Err(PackageError::TooLarge(format!(
                "{} entries (limit {MAX_ENTRIES})",
                archive.len()
            )));
        }
        let mut index = HashMap::new();
        let mut names = Vec::new();
        let mut total: u64 = 0;
        for i in 0..archive.len() {
            let file = archive
                .by_index_raw(i)
                .map_err(|e| PackageError::Corrupt(e.to_string()))?;
            total = total.saturating_add(file.size());
            if total > MAX_TOTAL_UNCOMPRESSED {
                return Err(PackageError::TooLarge(format!(
                    "more than {MAX_TOTAL_UNCOMPRESSED} bytes uncompressed"
                )));
            }
            if file.is_dir() {
                continue;
            }
            let name = String::from_utf8_lossy(file.name_raw()).replace('\\', "/");
            index.entry(normalize_name(&name)).or_insert(i);
            names.push(name);
        }
        Ok(Package {
            archive,
            index,
            names,
            read_total: 0,
        })
    }

    /// Names of all file entries (directories excluded) as stored.
    pub fn part_names(&self) -> &[String] {
        &self.names
    }

    /// True when a part with this name exists (case-insensitive, leading `/` ignored).
    pub fn contains(&self, name: &str) -> bool {
        self.index.contains_key(&normalize_name(name))
    }

    /// Reads a part. `Ok(None)` when absent. The actual decompressed length is enforced, so a
    /// lying size field cannot defeat the limit.
    pub fn read_part(&mut self, name: &str) -> Result<Option<Vec<u8>>, PackageError> {
        let Some(&i) = self.index.get(&normalize_name(name)) else {
            return Ok(None);
        };
        let file = self
            .archive
            .by_index(i)
            .map_err(|e| PackageError::Corrupt(e.to_string()))?;
        let remaining = MAX_TOTAL_UNCOMPRESSED.saturating_sub(self.read_total);
        let mut out = Vec::new();
        file.take(remaining + 1)
            .read_to_end(&mut out)
            .map_err(|e| PackageError::Corrupt(e.to_string()))?;
        if out.len() as u64 > remaining {
            return Err(PackageError::TooLarge(format!(
                "more than {MAX_TOTAL_UNCOMPRESSED} bytes uncompressed"
            )));
        }
        self.read_total += out.len() as u64;
        Ok(Some(out))
    }

    /// Reads and parses an XML part. `Ok(None)` when absent.
    pub fn read_xml(&mut self, name: &str) -> Result<Option<Element>, PackageError> {
        match self.read_part(name)? {
            Some(bytes) => Ok(Some(parse_xml(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Name of the main document part: the target of the package-level `officeDocument`
    /// relationship (`_rels/.rels`). `Ok(None)` when the package declares none.
    pub fn main_part(&mut self) -> Result<Option<String>, PackageError> {
        let Some(bytes) = self.read_part("_rels/.rels")? else {
            return Ok(None);
        };
        let rels = Relationships::parse(&bytes)?;
        Ok(rels
            .of_kind("officeDocument")
            .find(|r| !r.external)
            .map(|r| resolve_target("", &r.target)))
    }
}

/// One entry of a `.rels` part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub id: String,
    /// Full relationship type URI.
    pub kind: String,
    pub target: String,
    pub external: bool,
}

/// The relationships of one part.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Relationships {
    items: Vec<Relationship>,
}

impl Relationships {
    /// Parses a `.rels` part. Entries without `Id` or `Target` are skipped.
    pub fn parse(bytes: &[u8]) -> Result<Relationships, XmlError> {
        let root = parse_xml(bytes)?;
        let mut items = Vec::new();
        for e in root.elements().filter(|e| e.name == "Relationship") {
            let (Some(id), Some(target)) = (e.attr("Id"), e.attr("Target")) else {
                continue;
            };
            items.push(Relationship {
                id: id.to_string(),
                kind: e.attr("Type").unwrap_or_default().to_string(),
                target: target.to_string(),
                external: e
                    .attr("TargetMode")
                    .is_some_and(|m| m.eq_ignore_ascii_case("External")),
            });
        }
        Ok(Relationships { items })
    }

    /// Relationship with the given id.
    pub fn get(&self, id: &str) -> Option<&Relationship> {
        self.items.iter().find(|r| r.id == id)
    }

    /// All relationships whose type ends with `/<suffix>` (works for transitional and strict).
    pub fn of_kind<'s>(&'s self, suffix: &'s str) -> impl Iterator<Item = &'s Relationship> {
        self.items.iter().filter(move |r| kind_is(&r.kind, suffix))
    }

    /// First relationship of the given kind suffix.
    pub fn first_of_kind(&self, suffix: &str) -> Option<&Relationship> {
        self.items.iter().find(|r| kind_is(&r.kind, suffix))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

fn kind_is(kind: &str, suffix: &str) -> bool {
    kind.rsplit('/').next() == Some(suffix)
}

/// Path of the `.rels` part belonging to `part` (`word/document.xml` →
/// `word/_rels/document.xml.rels`).
pub fn rels_path_for(part: &str) -> String {
    match part.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{part}.rels"),
    }
}

/// Resolves a relationship `target` relative to the part `base_part`. Absolute targets (leading
/// `/`) are package-rooted; `.` and `..` segments are normalised; `..` cannot climb above the
/// package root.
pub fn resolve_target(base_part: &str, target: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let joined;
    let path = if let Some(abs) = target.strip_prefix('/') {
        abs
    } else {
        let dir = base_part.rsplit_once('/').map_or("", |(d, _)| d);
        joined = if dir.is_empty() {
            target.to_string()
        } else {
            format!("{dir}/{target}")
        };
        joined.as_str()
    };
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            s => segments.push(s),
        }
    }
    segments.join("/")
}

/// Escapes `s` for XML text or attribute values and drops characters XML 1.0 cannot carry.
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Interprets an `ST_OnOff` attribute value: an element that is present without a value is on;
/// `0`, `false` and `off` switch it off; anything else (`1`, `true`, `on`, …) is on.
pub fn on_off(value: Option<&str>) -> bool {
    !matches!(value, Some("0" | "false" | "off"))
}

/// Kind of file a byte stream is, by magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// ZIP archive (OPC packages, OpenDocument, …).
    Zip,
    /// OLE compound file that wraps a password-protected OOXML package.
    OleEncrypted,
    /// OLE compound file without an encrypted package (legacy binary Office format).
    Ole,
    Rtf,
    Pdf,
    Unknown,
}

impl Container {
    /// Noun phrase for error messages ("looks like …").
    pub fn describe(self) -> &'static str {
        match self {
            Container::Zip => "a ZIP archive",
            Container::OleEncrypted => "an encrypted document",
            Container::Ole => "a legacy binary Office document",
            Container::Rtf => "an RTF document",
            Container::Pdf => "a PDF document",
            Container::Unknown => "an unknown file type",
        }
    }
}

/// Classifies `bytes` by their leading magic number; compound files are opened to tell an
/// encrypted OOXML wrapper (`EncryptedPackage` stream) from a legacy binary document.
pub fn sniff_container(bytes: &[u8]) -> Container {
    const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if bytes.starts_with(b"PK\x03\x04")
        || bytes.starts_with(b"PK\x05\x06")
        || bytes.starts_with(b"PK\x07\x08")
    {
        Container::Zip
    } else if bytes.starts_with(&OLE_MAGIC) {
        let encrypted = cfb::CompoundFile::open(Cursor::new(bytes))
            .is_ok_and(|file| file.exists("/EncryptedPackage"));
        if encrypted {
            Container::OleEncrypted
        } else {
            Container::Ole
        }
    } else if bytes.starts_with(b"{\\rtf") {
        Container::Rtf
    } else if bytes.starts_with(b"%PDF") {
        Container::Pdf
    } else {
        Container::Unknown
    }
}

/// Raster formats Word embeds without conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
}

impl ImageFormat {
    pub fn mime(self) -> &'static str {
        match self {
            ImageFormat::Png => "image/png",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Gif => "image/gif",
        }
    }

    /// File extension without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Gif => "gif",
        }
    }
}

/// Format and pixel size of an image, read from its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: ImageFormat,
    pub width: u32,
    pub height: u32,
}

fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 2)?;
    Some(u32::from(u16::from_be_bytes([b[0], b[1]])))
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    loop {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        // Any number of 0xFF fill bytes may precede the marker code.
        while *bytes.get(at + 1)? == 0xFF {
            at += 1;
        }
        let marker = *bytes.get(at + 1)?;
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xD0..=0xD8 => at += 2,
            // End of image / start of scan before any frame header: no size.
            0xD9 | 0xDA => return None,
            // Start-of-frame markers (not DHT, JPG, DAC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                return Some((be16(bytes, at + 7)?, be16(bytes, at + 5)?));
            }
            _ => at += 2 + usize::try_from(be16(bytes, at + 2)?).ok()?,
        }
    }
}

/// Format and pixel size of a PNG, JPEG or GIF image; `None` for other data or a zero size.
pub fn image_info(bytes: &[u8]) -> Option<ImageInfo> {
    let (format, (width, height)) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if bytes.get(12..16) != Some(b"IHDR".as_slice()) {
            return None;
        }
        (ImageFormat::Png, (be32(bytes, 16)?, be32(bytes, 20)?))
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        (ImageFormat::Jpeg, jpeg_size(bytes)?)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        let b = bytes.get(6..10)?;
        (
            ImageFormat::Gif,
            (
                u32::from(u16::from_le_bytes([b[0], b[1]])),
                u32::from(u16::from_le_bytes([b[2], b[3]])),
            ),
        )
    } else {
        return None;
    };
    (width > 0 && height > 0).then_some(ImageInfo {
        format,
        width,
        height,
    })
}

/// Builder for `[Content_Types].xml`.
#[derive(Debug, Clone, Default)]
pub struct ContentTypes {
    defaults: Vec<(String, String)>,
    overrides: Vec<(String, String)>,
}

impl ContentTypes {
    pub fn new() -> ContentTypes {
        ContentTypes::default()
    }

    /// Registers a content type for a file extension (no leading dot); duplicates are ignored.
    pub fn default_type(&mut self, extension: &str, content_type: &str) {
        let ext = extension.to_ascii_lowercase();
        if !self.defaults.iter().any(|(e, _)| *e == ext) {
            self.defaults.push((ext, content_type.to_string()));
        }
    }

    /// Registers a content type for one part (`/word/document.xml`).
    pub fn override_part(&mut self, part_name: &str, content_type: &str) {
        self.overrides
            .push((part_name.to_string(), content_type.to_string()));
    }

    /// Serialises the part.
    pub fn to_xml(&self) -> String {
        let mut x = format!(r#"{XML_DECL}<Types xmlns="{}">"#, uri::CONTENT_TYPES);
        for (ext, ct) in &self.defaults {
            x.push_str(&format!(
                r#"<Default Extension="{}" ContentType="{}"/>"#,
                escape_xml(ext),
                escape_xml(ct)
            ));
        }
        for (part, ct) in &self.overrides {
            x.push_str(&format!(
                r#"<Override PartName="{}" ContentType="{}"/>"#,
                escape_xml(part),
                escape_xml(ct)
            ));
        }
        x.push_str("</Types>");
        x
    }
}

/// Builder for a `.rels` part. Ids are `rId1`, `rId2`, … in insertion order.
#[derive(Debug, Clone, Default)]
pub struct RelationshipsBuilder {
    items: Vec<Relationship>,
}

impl RelationshipsBuilder {
    pub fn new() -> RelationshipsBuilder {
        RelationshipsBuilder::default()
    }

    /// Adds a relationship and returns its id.
    pub fn add(&mut self, kind: &str, target: &str, external: bool) -> String {
        let id = format!("rId{}", self.items.len() + 1);
        self.items.push(Relationship {
            id: id.clone(),
            kind: kind.to_string(),
            target: target.to_string(),
            external,
        });
        id
    }

    /// Like [`add`](Self::add) but reuses an existing identical relationship.
    pub fn add_unique(&mut self, kind: &str, target: &str, external: bool) -> String {
        match self
            .items
            .iter()
            .find(|r| r.kind == kind && r.target == target && r.external == external)
        {
            Some(r) => r.id.clone(),
            None => self.add(kind, target, external),
        }
    }

    /// Serialises the part.
    pub fn to_xml(&self) -> String {
        let mut x = format!(r#"{XML_DECL}<Relationships xmlns="{}">"#, uri::PKG_REL);
        for r in &self.items {
            x.push_str(&format!(
                r#"<Relationship Id="{}" Type="{}" Target="{}"{}/>"#,
                escape_xml(&r.id),
                escape_xml(&r.kind),
                escape_xml(&r.target),
                if r.external {
                    r#" TargetMode="External""#
                } else {
                    ""
                }
            ));
        }
        x.push_str("</Relationships>");
        x
    }
}

/// Collects the parts of a ZIP package (deflate-compressed, no directory entries) and builds
/// the archive in memory.
///
/// The archive is never written to a caller-supplied sink part by part: the `zip` crate does
/// not survive an I/O error in the middle of a file (it patches local headers by seeking back
/// and re-checks stream positions when dropped), so all I/O failures are left to whoever
/// stores the finished bytes.
#[derive(Default)]
pub struct PackageWriter {
    parts: Vec<(String, Vec<u8>)>,
}

impl std::fmt::Debug for PackageWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.parts.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("PackageWriter")
            .field("parts", &names)
            .finish()
    }
}

impl PackageWriter {
    pub fn new() -> PackageWriter {
        PackageWriter::default()
    }

    /// Adds one part. Part names must be unique; a duplicate is reported by [`finish`](Self::finish).
    pub fn add(&mut self, name: &str, data: impl Into<Vec<u8>>) {
        self.parts.push((name.to_string(), data.into()));
    }

    /// Builds the archive and returns its bytes.
    pub fn finish(self) -> Result<Vec<u8>, PackageError> {
        zip_parts(&self.parts).map_err(|e| PackageError::Io(e.to_string()))
    }
}

fn zip_parts(parts: &[(String, Vec<u8>)]) -> zip::result::ZipResult<Vec<u8>> {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in parts {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        zip.start_file(name.as_str(), options)
            .and_then(|()| zip.write_all(data).map_err(zip::result::ZipError::from))?;
    }
    zip.finish().map(Cursor::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W_NS: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
    const STRICT_W_NS: &str = "http://purl.oclc.org/ooxml/wordprocessingml/main";

    #[test]
    fn parse_resolves_namespaces_attributes_and_entities() {
        let xml = format!(
            r#"<?xml version="1.0"?><x:doc xmlns:x="{W_NS}" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><x:p x:val="a&amp;b&#10;c" m:val="2" plain="3" u:q="4">Tom &amp; Jerry &#x41;&lt;<![CDATA[<raw>]]></x:p><m:r/><plain/></x:doc>"#
        );
        let root = parse_xml(xml.as_bytes()).unwrap();
        assert!(root.is(Ns::W, "doc"));
        assert!(
            root.attrs.is_empty(),
            "xmlns declarations are not attributes"
        );
        let p = root.child(Ns::W, "p").unwrap();
        assert_eq!(p.attr("val"), Some("a&b\nc"));
        assert_eq!(p.attr_ns(Ns::W, "val"), Some("a&b\nc"));
        assert_eq!(p.attr_ns(Ns::M, "val"), Some("2"));
        assert_eq!(p.attr_ns(Ns::None, "plain"), Some("3"));
        assert_eq!(p.attr_ns(Ns::Other, "q"), Some("4"));
        assert_eq!(p.attr_ns(Ns::W, "plain"), None);
        assert_eq!(p.attr("missing"), None);
        assert_eq!(p.text(), "Tom & Jerry A<<raw>");
        assert!(root.child(Ns::M, "r").is_some());
        assert_eq!(root.child(Ns::None, "plain").unwrap().ns, Ns::None);
        assert_eq!(root.elements().count(), 3);
        assert!(root.child(Ns::W, "missing").is_none());
    }

    #[test]
    fn descendants_are_collected_at_any_depth_in_document_order() {
        let xml = format!(
            r#"<w:a xmlns:w="{W_NS}"><w:p n="1"><w:p n="2"/></w:p><w:q><w:p n="3"/></w:q><w:p n="4"/></w:a>"#
        );
        let root = parse_xml(xml.as_bytes()).unwrap();
        let numbers: Vec<_> = root
            .descendants(Ns::W, "p")
            .iter()
            .map(|e| e.attr("n").unwrap().to_string())
            .collect();
        assert_eq!(numbers, ["1", "2", "3", "4"]);
        assert!(root.descendants(Ns::M, "p").is_empty());
    }

    #[test]
    fn salvage_keeps_the_prefix_of_damaged_input() {
        let xml = format!(r#"<w:a xmlns:w="{W_NS}"><w:b>Kept<w:c>deep"#);
        let (root, damaged) = parse_xml_salvage(xml.as_bytes()).unwrap();
        assert!(damaged);
        assert_eq!(root.text(), "Keptdeep");
        assert!(root.child(Ns::W, "b").unwrap().child(Ns::W, "c").is_some());

        let (root, damaged) = parse_xml_salvage(b"<a><b/></a>").unwrap();
        assert!(!damaged);
        assert!(root.child(Ns::None, "b").is_some());

        // A syntax error mid-document, an end tag without start and a second root all stop the parse.
        for input in [&b"<a><b/><c></d>tail</a>"[..], b"<a/></a>", b"<a/><b/>"] {
            let (root, damaged) = parse_xml_salvage(input).unwrap();
            assert!(damaged, "{}", String::from_utf8_lossy(input));
            assert_eq!(root.name, "a");
        }
        assert!(parse_xml_salvage(b"").is_none());
        assert!(parse_xml_salvage(b"<<<").is_none());
        let deep = "<a>".repeat(MAX_XML_DEPTH + 1);
        assert!(parse_xml_salvage(deep.as_bytes()).is_none());
    }

    #[test]
    fn on_off_follows_st_onoff() {
        assert!(on_off(None));
        for on in ["1", "true", "on", ""] {
            assert!(on_off(Some(on)), "{on}");
        }
        for off in ["0", "false", "off"] {
            assert!(!on_off(Some(off)), "{off}");
        }
    }

    #[test]
    fn strict_namespaces_map_to_the_same_ns() {
        let xml = format!(r#"<w:p xmlns:w="{STRICT_W_NS}"/>"#);
        assert!(parse_xml(xml.as_bytes()).unwrap().is(Ns::W, "p"));
    }

    #[test]
    fn ns_from_uri_covers_every_known_namespace() {
        let cases: [(&str, Ns); 13] = [
            (uri::W, Ns::W),
            (uri::M, Ns::M),
            (uri::A, Ns::A),
            (uri::WP, Ns::Wp),
            (uri::PIC, Ns::Pic),
            (uri::R, Ns::R),
            ("urn:schemas-microsoft-com:vml", Ns::V),
            ("urn:schemas-microsoft-com:office:office", Ns::O),
            (
                "http://schemas.openxmlformats.org/markup-compatibility/2006",
                Ns::Mc,
            ),
            (
                "http://schemas.microsoft.com/office/word/2010/wordprocessingShape",
                Ns::Wps,
            ),
            (uri::PKG_REL, Ns::PkgRel),
            ("http://purl.oclc.org/ooxml/drawingml/main", Ns::A),
            ("urn:other", Ns::Other),
        ];
        for (u, expected) in cases {
            assert_eq!(Ns::from_uri(u), expected, "{u}");
        }
    }

    #[test]
    fn parse_accepts_bom_and_rejects_bad_input() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"<a><b/></a>");
        assert!(parse_xml(&bytes).unwrap().child(Ns::None, "b").is_some());
        assert!(matches!(
            parse_xml(b"<a><b></a>"),
            Err(XmlError::Malformed(_))
        ));
        assert!(matches!(parse_xml(b"<a>"), Err(XmlError::Malformed(_))));
        assert!(matches!(parse_xml(b"</a>"), Err(XmlError::Malformed(_))));
        assert!(matches!(
            parse_xml(b"<a/><b/>"),
            Err(XmlError::Malformed(_))
        ));
        assert_eq!(parse_xml(b"   "), Err(XmlError::Empty));
    }

    #[test]
    fn parse_rejects_excessive_depth_but_accepts_the_limit() {
        let nest = |n: usize| "<a>".repeat(n) + &"</a>".repeat(n);
        assert!(parse_xml(nest(MAX_XML_DEPTH).as_bytes()).is_ok());
        assert_eq!(
            parse_xml(nest(MAX_XML_DEPTH + 1).as_bytes()),
            Err(XmlError::TooDeep)
        );
    }

    #[test]
    fn rels_parse_lookup_and_kinds() {
        let xml = format!(
            r#"<Relationships xmlns="{}"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/><Relationship Id="rId2" Type="http://purl.oclc.org/ooxml/officeDocument/relationships/hyperlink" Target="https://x.test/?a=1&amp;b=2" TargetMode="External"/><Relationship Id="rId3"/></Relationships>"#,
            uri::PKG_REL
        );
        let rels = Relationships::parse(xml.as_bytes()).unwrap();
        assert_eq!(rels.len(), 2);
        assert!(!rels.is_empty());
        assert_eq!(rels.get("rId1").unwrap().target, "styles.xml");
        assert!(!rels.get("rId1").unwrap().external);
        let link = rels.first_of_kind("hyperlink").unwrap();
        assert_eq!(link.target, "https://x.test/?a=1&b=2");
        assert!(link.external);
        assert!(rels.first_of_kind("numbering").is_none());
        assert_eq!(rels.of_kind("styles").count(), 1);
        assert!(rels.get("rId3").is_none());
        assert!(Relationships::default().is_empty());
    }

    #[test]
    fn rels_path_and_target_resolution() {
        assert_eq!(
            rels_path_for("word/document.xml"),
            "word/_rels/document.xml.rels"
        );
        assert_eq!(rels_path_for("document.xml"), "_rels/document.xml.rels");
        let cases = [
            ("word/document.xml", "media/a.png", "word/media/a.png"),
            ("word/document.xml", "/word/styles.xml", "word/styles.xml"),
            (
                "word/document.xml",
                "../customXml/item1.xml",
                "customXml/item1.xml",
            ),
            ("word/document.xml", "./a/../b.png", "word/b.png"),
            ("document.xml", "media/a.png", "media/a.png"),
            ("word/document.xml", "../../../x.png", "x.png"),
        ];
        for (base, target, expected) in cases {
            assert_eq!(resolve_target(base, target), expected, "{base} + {target}");
        }
    }

    #[test]
    fn escape_xml_escapes_and_drops_invalid_characters() {
        assert_eq!(
            escape_xml("a<b>&\"c\"'d' \u{1}\u{b}\u{fffe}\t\n\r é"),
            "a&lt;b&gt;&amp;&quot;c&quot;&apos;d&apos; \t\n\r é"
        );
    }

    #[test]
    fn builders_emit_parseable_xml() {
        let mut ct = ContentTypes::new();
        ct.default_type("PNG", "image/png");
        ct.default_type("png", "image/other");
        ct.override_part("/word/document.xml", "application/x+xml");
        let root = parse_xml(ct.to_xml().as_bytes()).unwrap();
        assert!(root.is(Ns::Other, "Types"));
        let defaults: Vec<_> = root.elements().filter(|e| e.name == "Default").collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].attr("Extension"), Some("png"));
        assert_eq!(defaults[0].attr("ContentType"), Some("image/png"));
        let o = root.child(Ns::Other, "Override").unwrap();
        assert_eq!(o.attr("PartName"), Some("/word/document.xml"));

        let mut rb = RelationshipsBuilder::new();
        let a = rb.add(rel::STYLES, "styles.xml", false);
        let b = rb.add(rel::HYPERLINK, "https://x.test/?a=1&b=2", true);
        let c = rb.add_unique(rel::HYPERLINK, "https://x.test/?a=1&b=2", true);
        let d = rb.add_unique(rel::HYPERLINK, "https://y.test/", true);
        assert_eq!(
            (a.as_str(), b.as_str(), c.as_str(), d.as_str()),
            ("rId1", "rId2", "rId2", "rId3")
        );
        let rels = Relationships::parse(rb.to_xml().as_bytes()).unwrap();
        assert_eq!(rels.len(), 3);
        assert_eq!(rels.get("rId2").unwrap().target, "https://x.test/?a=1&b=2");
        assert!(rels.get("rId2").unwrap().external);
        assert!(!rels.get("rId1").unwrap().external);
    }

    #[test]
    fn package_round_trip_is_case_insensitive_and_skips_directories() {
        let mut w = PackageWriter::default();
        w.add("Word/Document.xml", b"<a/>");
        w.add("docProps/", b"");
        w.add("a\\b.txt", b"hi");
        let bytes = w.finish().unwrap();
        let mut p = Package::open(&bytes).unwrap();
        assert!(p.contains("/word/document.xml"));
        assert!(!p.contains("missing"));
        assert_eq!(p.part_names().len(), 2);
        assert_eq!(p.read_part("word/document.xml").unwrap().unwrap(), b"<a/>");
        assert_eq!(p.read_part("a/b.txt").unwrap().unwrap(), b"hi");
        assert_eq!(p.read_part("nope").unwrap(), None);
        assert!(
            p.read_xml("word/document.xml")
                .unwrap()
                .unwrap()
                .is(Ns::None, "a")
        );
        assert!(p.read_xml("nope").unwrap().is_none());
        p.read_part("a/b.txt").unwrap();
        assert!(format!("{:?}", PackageWriter::new()).contains("PackageWriter"));
    }

    #[test]
    fn package_writer_accepts_borrowed_and_owned_data_and_rejects_duplicates() {
        let mut w = PackageWriter::new();
        w.add("a.txt", b"hello");
        w.add("b.txt", "text".to_string().into_bytes());
        w.add("c.txt", &b"slice"[..]);
        assert_eq!(
            format!("{w:?}"),
            r#"PackageWriter { parts: ["a.txt", "b.txt", "c.txt"] }"#
        );
        let bytes = w.finish().unwrap();
        let mut p = Package::open(&bytes).unwrap();
        assert_eq!(p.read_part("a.txt").unwrap().unwrap(), b"hello");
        assert_eq!(p.read_part("b.txt").unwrap().unwrap(), b"text");
        assert_eq!(p.read_part("c.txt").unwrap().unwrap(), b"slice");

        let mut dup = PackageWriter::new();
        dup.add("a.txt", b"1");
        dup.add("a.txt", b"2");
        let error = dup.finish().unwrap_err();
        assert!(
            matches!(&error, PackageError::Io(m) if m.contains("Duplicate")),
            "{error}"
        );
    }

    #[test]
    fn package_errors_describe_themselves() {
        let cases = [
            (
                PackageError::Corrupt("x".into()),
                "not a valid ZIP package: x",
            ),
            (
                PackageError::TooLarge("y".into()),
                "package exceeds the size limit: y",
            ),
            (PackageError::Io("z".into()), "package I/O failure: z"),
            (
                PackageError::Xml(XmlError::TooDeep),
                "XML nesting is deeper than 128 levels",
            ),
            (
                PackageError::Xml(XmlError::Malformed("m".into())),
                "malformed XML: m",
            ),
            (
                PackageError::Xml(XmlError::Empty),
                "XML document has no root element",
            ),
        ];
        for (error, text) in cases {
            assert_eq!(error.to_string(), text);
        }
    }

    #[test]
    fn read_xml_reports_malformed_parts() {
        let mut w = PackageWriter::new();
        w.add("bad.xml", b"<a><b></a>");
        let bytes = w.finish().unwrap();
        let mut p = Package::open(&bytes).unwrap();
        assert!(matches!(
            p.read_xml("bad.xml"),
            Err(PackageError::Xml(XmlError::Malformed(_)))
        ));
    }

    fn ole_with_stream(name: &str) -> Vec<u8> {
        let mut file = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        let mut stream = file.create_stream(name).unwrap();
        stream.write_all(b"data").unwrap();
        drop(stream);
        file.flush().unwrap();
        file.into_inner().into_inner()
    }

    #[test]
    fn sniff_container_classifies_by_magic() {
        let encrypted = ole_with_stream("/EncryptedPackage");
        let legacy = ole_with_stream("/WordDocument");
        let cases: [(&[u8], Container, &str); 10] = [
            (b"PK\x03\x04rest", Container::Zip, "a ZIP archive"),
            (b"PK\x05\x06", Container::Zip, "a ZIP archive"),
            (b"PK\x07\x08", Container::Zip, "a ZIP archive"),
            (&encrypted, Container::OleEncrypted, "an encrypted document"),
            (&legacy, Container::Ole, "a legacy binary Office document"),
            (
                &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0],
                Container::Ole,
                "a legacy binary Office document",
            ),
            (b"{\\rtf1\\ansi}", Container::Rtf, "an RTF document"),
            (b"%PDF-1.7", Container::Pdf, "a PDF document"),
            (b"hello", Container::Unknown, "an unknown file type"),
            (b"", Container::Unknown, "an unknown file type"),
        ];
        for (bytes, expected, text) in cases {
            let kind = sniff_container(bytes);
            assert_eq!(kind, expected, "{bytes:?}");
            assert_eq!(kind.describe(), text);
        }
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn jpeg(segments: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        bytes.extend_from_slice(segments);
        bytes
    }

    #[test]
    fn image_info_reads_png_jpeg_and_gif_headers() {
        let gif = [b"GIF89a".as_slice(), &[3, 0, 2, 0, 0, 0, 0]].concat();
        // APP0 segment (length 4), fill bytes, then SOF0: 8 bit, height 5, width 7.
        let sof = jpeg(&[
            0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xFF, 0xC0, 0, 11, 8, 0, 5, 0, 7, 1, 1, 0x11, 0,
        ]);
        // RST0 (standalone), an empty DHT segment, then a progressive frame: 256 high, 512 wide.
        let progressive = jpeg(&[
            0xFF, 0xD0, 0xFF, 0xC4, 0, 2, 0xFF, 0xC2, 0, 11, 8, 1, 0, 2, 0, 1, 1, 0x11, 0,
        ]);
        let cases = [
            (
                png(640, 480),
                ImageFormat::Png,
                640,
                480,
                "image/png",
                "png",
            ),
            (gif, ImageFormat::Gif, 3, 2, "image/gif", "gif"),
            (sof, ImageFormat::Jpeg, 7, 5, "image/jpeg", "jpeg"),
            (
                progressive,
                ImageFormat::Jpeg,
                512,
                256,
                "image/jpeg",
                "jpeg",
            ),
        ];
        for (bytes, format, width, height, mime, extension) in cases {
            assert_eq!(
                image_info(&bytes),
                Some(ImageInfo {
                    format,
                    width,
                    height
                })
            );
            assert_eq!(format.mime(), mime);
            assert_eq!(format.extension(), extension);
        }
    }

    #[test]
    fn image_info_rejects_unknown_truncated_and_empty_images() {
        let mut no_ihdr = png(1, 1);
        no_ihdr[12] = b'X';
        let cases: [(&str, Vec<u8>); 12] = [
            ("empty", vec![]),
            ("text", b"hello world".to_vec()),
            ("png without IHDR", no_ihdr),
            ("png truncated", png(1, 1)[..20].to_vec()),
            ("png zero width", png(0, 4)),
            ("png zero height", png(4, 0)),
            ("gif truncated", b"GIF89a\x01".to_vec()),
            ("jpeg ends", jpeg(&[0xFF, 0xD9])),
            ("jpeg scan before frame", jpeg(&[0xFF, 0xDA, 0, 2])),
            ("jpeg garbage", jpeg(&[0x12, 0x34])),
            ("jpeg truncated segment", jpeg(&[0xFF, 0xE0, 0xFF])),
            ("jpeg truncated frame", jpeg(&[0xFF, 0xC0, 0, 11, 8, 0, 5])),
        ];
        for (label, bytes) in cases {
            assert_eq!(image_info(&bytes), None, "{label}");
        }
    }

    #[test]
    fn main_part_follows_the_office_document_relationship() {
        let build = |rels: Option<&str>| {
            let mut w = PackageWriter::new();
            w.add("word/document.xml", b"<d/>");
            if let Some(rels) = rels {
                w.add("_rels/.rels", rels.as_bytes());
            }
            w.finish().unwrap()
        };
        let mut b = RelationshipsBuilder::new();
        b.add(rel::HYPERLINK, "https://x.test/", true);
        b.add(rel::OFFICE_DOCUMENT, "/word/document.xml", false);
        let with = build(Some(&b.to_xml()));
        assert_eq!(
            Package::open(&with)
                .unwrap()
                .main_part()
                .unwrap()
                .as_deref(),
            Some("word/document.xml")
        );
        let without = build(None);
        assert_eq!(Package::open(&without).unwrap().main_part().unwrap(), None);
        let broken = build(Some("<Relationships>"));
        assert!(matches!(
            Package::open(&broken).unwrap().main_part(),
            Err(PackageError::Xml(_))
        ));
        let mut only_external = RelationshipsBuilder::new();
        only_external.add(rel::OFFICE_DOCUMENT, "https://x.test/d", true);
        let external = build(Some(&only_external.to_xml()));
        assert_eq!(Package::open(&external).unwrap().main_part().unwrap(), None);
    }

    #[test]
    fn open_rejects_non_zip_and_too_many_entries() {
        assert!(matches!(
            Package::open(b"definitely not a zip"),
            Err(PackageError::Corrupt(_))
        ));
        let mut w = PackageWriter::new();
        for i in 0..=MAX_ENTRIES {
            w.add(&format!("f{i}.txt"), b"");
        }
        let bytes = w.finish().unwrap();
        assert!(matches!(
            Package::open(&bytes),
            Err(PackageError::TooLarge(_))
        ));
    }
}
