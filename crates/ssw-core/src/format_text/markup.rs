//! XML and HTML: conservative re-indentation of element structure.
//!
//! The document is parsed into a flat node tree (iteratively, with a depth cap), then printed
//! as a list of pieces: byte ranges of the source that are copied verbatim, plus the line
//! breaks and indents the formatter generates. An element is *expandable* — each child on its
//! own line, the end tag on its own line — only when the whitespace between its children
//! cannot be content:
//!
//! * every child is an element, a comment or a processing instruction (no text, no CDATA);
//! * in XML every whitespace gap between the children contains a line break, so a gap such as
//!   `<a>x</a> <b>y</b>` stays data. Touching children without any gap cannot be told from
//!   structure in generic XML, so they are split — except in the elements of `INLINE_XML`,
//!   which render their children inline;
//! * in HTML every child element is block-level — a known `display: block`, table or
//!   `display: none` element that has no `style` attribute mentioning `display`. An
//!   inline-level or unknown child keeps the whole container verbatim, because whitespace
//!   between inline elements is rendered. Only the user-agent default `display` and an inline
//!   `style` are known: stylesheets and classes are not consulted;
//! * the element is not `pre`, `textarea`, `script`, `style` (or, in HTML, `template`; in XML,
//!   one of `INLINE_XML`), has no `xml:space="preserve"` and no `style` attribute that
//!   mentions `white-space` or `display`.
//!
//! Every other element — text-only, mixed, preserved, childless — is copied byte for byte
//! from its start tag to its end tag. Start tags, end tags, attributes, comments, processing
//! instructions, CDATA and the DOCTYPE are always copied verbatim. The generated whitespace
//! replaces only whitespace-only text between nodes, with at most one blank line kept.
//!
//! The HTML parser follows the HTML tokenizer where it matters for finding where things end:
//! raw-text elements (`script`, `style`, `textarea`, `title`, …) end at their end tag, `<?…>`
//! and `<![CDATA[…>` are bogus comments, `/>` closes only void and SVG/MathML elements. It
//! does not infer omitted end tags: such documents are refused as unbalanced. XML must be well
//! formed with exactly one root element; an internal DTD subset (which may declare
//! `xml:space` defaults) keeps the root element verbatim.

use super::{
    EolCounts, FormatIndent, FormatOutcome, Refusal, RefusalReason, bom_len, contains_ci, find,
    finish, line_ending,
};

/// Elements nested deeper than this are refused.
const MAX_DEPTH: usize = 256;

const NONE: usize = usize::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dialect {
    Xml,
    Html,
}

/// HTML elements whose whitespace-only neighbours are not rendered: block-level, table and
/// `display: none` elements.
const BLOCK: &[&str] = &[
    "html",
    "head",
    "body",
    "address",
    "article",
    "aside",
    "blockquote",
    "details",
    "dialog",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "legend",
    "main",
    "menu",
    "nav",
    "ol",
    "p",
    "pre",
    "search",
    "section",
    "summary",
    "ul",
    "li",
    "table",
    "caption",
    "colgroup",
    "col",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "center",
    "meta",
    "link",
    "title",
    "base",
    "script",
    "style",
    "template",
];

/// HTML elements without content and end tag.
const VOID: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

/// HTML elements whose content is text up to the matching end tag.
const RAW_TEXT: &[&str] = &[
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes",
];

/// XML elements that render their children inline, the way HTML's `span` does: whitespace
/// between two children is shown as a space, and none is shown as none, so a line break put
/// between touching children (`<tspan>a</tspan><tspan>b</tspan>` reads `ab`) changes what the
/// document displays. Generic XML has no such knowledge, so the vocabularies that need it are
/// listed by name, the way HTML has its block list. Anything not listed is laid out as plain
/// structure; `xml:space="preserve"` protects an element of any other vocabulary.
const INLINE_XML: &[&str] = &[
    // SVG text.
    "text",
    "tspan",
    "textPath",
    "title",
    "desc",
    // XAML text: text blocks and the inline elements of flow documents.
    "TextBlock",
    "Paragraph",
    "Span",
    "Run",
    "Bold",
    "Italic",
    "Underline",
    "Hyperlink",
];

fn name_in(name: &[u8], list: &[&str]) -> bool {
    list.iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate.as_bytes()))
}

/// Whether the XML element `name` is one of `INLINE_XML`. Names are case-sensitive, as in
/// XML; a namespace prefix (`svg:text`) is ignored.
fn is_inline_xml(name: &[u8]) -> bool {
    let local = name
        .iter()
        .rposition(|&byte| byte == b':')
        .map_or(name, |colon| &name[colon + 1..]);
    INLINE_XML
        .iter()
        .any(|candidate| local == candidate.as_bytes())
}

/// Elements whose content is never touched.
fn keeps_whitespace(name: &[u8], html: bool) -> bool {
    name_in(name, &["pre", "textarea", "script", "style"])
        || if html {
            name_in(name, &["template", "listing", "xmp"])
        } else {
            is_inline_xml(name)
        }
}

/// Whitespace inside a tag.
const fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

fn starts_with_ci(bytes: &[u8], prefix: &[u8]) -> bool {
    bytes
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

pub(super) fn format(text: &str, indent: FormatIndent, dialect: Dialect) -> FormatOutcome {
    match run(text, indent, dialect == Dialect::Html) {
        Ok(formatted) => finish(text, formatted),
        Err(refusal) => FormatOutcome::Refused(refusal),
    }
}

fn run(text: &str, indent: FormatIndent, html: bool) -> Result<String, Refusal> {
    let bom = bom_len(text);
    let mut parser = Parser::new(text.as_bytes(), html, bom);
    parser.parse()?;
    parser.check_document()?;
    let mut planner = Planner {
        bytes: text.as_bytes(),
        nodes: &parser.nodes,
        html,
        pieces: Vec::new(),
        gaps: EolCounts::default(),
    };
    planner.raw(0, bom);
    if planner.expandable(0) {
        planner.plan_children(0, 0);
    } else {
        planner.raw(bom, text.len());
    }
    let eol = line_ending(planner.gaps, text);
    Ok(render(text, &planner.pieces, eol, indent.unit()))
}

// ------------------------------------------------------------------------------------- tree

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    /// The root; its children are the top-level nodes.
    Document,
    Element,
    /// Text with something other than whitespace in it.
    Text,
    /// Whitespace-only text.
    Blank,
    /// A comment, and in HTML every bogus comment (`<?…>`, `<!…>`, `</…>`).
    Comment,
    /// An XML processing instruction, the XML declaration included.
    Pi,
    Doctype,
    Cdata,
}

#[derive(Clone, Copy, Debug)]
struct Node {
    kind: NodeKind,
    start: usize,
    /// One past the last byte; for an element, the end of its end tag.
    end: usize,
    /// Elements: end of the start tag.
    open_end: usize,
    /// Elements: start of the end tag; equal to `end` when there is none.
    close_start: usize,
    first_child: usize,
    last_child: usize,
    next: usize,
    /// Elements: the content must not be touched.
    verbatim: bool,
    /// Elements: whitespace-only text next to the element is not rendered (always in XML; in
    /// HTML a known block-level element whose `style` does not mention `display`).
    block: bool,
}

impl Node {
    const fn leaf(kind: NodeKind, start: usize, end: usize) -> Self {
        Self {
            kind,
            start,
            end,
            open_end: end,
            close_start: end,
            first_child: NONE,
            last_child: NONE,
            next: NONE,
            verbatim: false,
            block: false,
        }
    }
}

/// The children of `parent`, in document order.
fn children(nodes: &[Node], parent: usize) -> impl Iterator<Item = usize> + '_ {
    std::iter::successors(
        Some(nodes[parent].first_child).filter(|&child| child != NONE),
        move |&child| Some(nodes[child].next).filter(|&next| next != NONE),
    )
}

// ---------------------------------------------------------------------------------- parser

/// An element whose end tag has not been seen yet.
#[derive(Clone, Copy, Debug)]
struct Open {
    node: usize,
    name_start: usize,
    name_end: usize,
    /// An HTML `svg` or `math` element: everything inside follows XML rules.
    foreign_root: bool,
}

/// What a tag scan found after the tag name.
#[derive(Clone, Copy, Debug)]
struct TagEnd {
    /// One past the closing `>`.
    end: usize,
    /// The tag ends in `/>`.
    self_closing: bool,
    /// `xml:space="preserve"`.
    preserve: bool,
    /// A `style` attribute that mentions `white-space`: the content is laid out by CSS.
    white_space: bool,
    /// A `style` attribute that mentions `display`: the content is laid out by CSS, and the
    /// element may be inline-level, so it is no block for its parent either.
    display: bool,
}

struct Parser<'a> {
    bytes: &'a [u8],
    html: bool,
    pos: usize,
    nodes: Vec<Node>,
    open: Vec<Open>,
    /// Number of open `svg`/`math` elements (HTML only).
    foreign: usize,
    internal_subset: bool,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8], html: bool, start: usize) -> Self {
        let mut document = Node::leaf(NodeKind::Document, start, bytes.len());
        document.block = true;
        Self {
            bytes,
            html,
            pos: start,
            nodes: vec![document],
            open: Vec::new(),
            foreign: 0,
            internal_subset: false,
        }
    }

    fn parse(&mut self) -> Result<(), Refusal> {
        while self.pos < self.bytes.len() {
            if self.bytes[self.pos] == b'<' && self.starts_markup(self.pos) {
                self.markup()?;
            } else {
                self.text_run();
            }
        }
        if let Some(open) = self.open.last() {
            return Err(Refusal::at(
                RefusalReason::UnclosedTag,
                self.nodes[open.node].start,
            ));
        }
        Ok(())
    }

    /// Checks the top level: something to format; in XML exactly one root element and no text
    /// beside it. An internal DTD subset makes the root element verbatim.
    fn check_document(&mut self) -> Result<(), Refusal> {
        let mut roots = 0;
        let mut content = false;
        let mut child = self.nodes[0].first_child;
        while child != NONE {
            let node = self.nodes[child];
            match node.kind {
                NodeKind::Element => {
                    roots += 1;
                    if !self.html && roots > 1 {
                        return Err(Refusal::at(RefusalReason::ContentOutsideRoot, node.start));
                    }
                    if self.internal_subset {
                        self.nodes[child].verbatim = true;
                    }
                    content = true;
                }
                NodeKind::Text | NodeKind::Cdata => {
                    if !self.html {
                        return Err(Refusal::at(RefusalReason::ContentOutsideRoot, node.start));
                    }
                    content = true;
                }
                NodeKind::Blank => {}
                _ => content = true,
            }
            child = node.next;
        }
        if !content || (!self.html && roots == 0) {
            return Err(Refusal::at(RefusalReason::EmptyDocument, self.bytes.len()));
        }
        Ok(())
    }

    /// Whether the `<` at `i` starts markup. In XML every `<` does (and is an error when the
    /// markup is malformed); in HTML a `<` that no tag, comment or declaration follows is text.
    fn starts_markup(&self, i: usize) -> bool {
        if !self.html {
            return true;
        }
        match self.bytes.get(i + 1) {
            Some(b'!' | b'?' | b'%') => true,
            Some(b'/') => i + 2 < self.bytes.len(),
            Some(byte) => byte.is_ascii_alphabetic(),
            None => false,
        }
    }

    fn append(&mut self, node: Node) -> usize {
        let index = self.nodes.len();
        let parent = self.open.last().map_or(0, |open| open.node);
        self.nodes.push(node);
        let last = self.nodes[parent].last_child;
        if last == NONE {
            self.nodes[parent].first_child = index;
        } else {
            self.nodes[last].next = index;
        }
        self.nodes[parent].last_child = index;
        index
    }

    fn leaf(&mut self, kind: NodeKind, start: usize, end: usize) -> Result<(), Refusal> {
        self.append(Node::leaf(kind, start, end));
        self.pos = end;
        Ok(())
    }

    fn text_run(&mut self) {
        let start = self.pos;
        let mut end = start + 1;
        while end < self.bytes.len() && !(self.bytes[end] == b'<' && self.starts_markup(end)) {
            end += 1;
        }
        let blank = self.bytes[start..end]
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        let kind = if blank {
            NodeKind::Blank
        } else {
            NodeKind::Text
        };
        self.append(Node::leaf(kind, start, end));
        self.pos = end;
    }

    /// Parses the markup construct at `<`.
    fn markup(&mut self) -> Result<(), Refusal> {
        let at = self.pos;
        match self.bytes.get(at + 1) {
            Some(b'!') => self.declaration(),
            Some(b'?') => self.instruction(),
            Some(b'/') => self.end_tag(),
            Some(b'%') => Err(Refusal::at(RefusalReason::UnsupportedSyntax, at)),
            _ => self.start_tag(),
        }
    }

    /// `<!--`, `<![CDATA[`, `<!DOCTYPE` and, in HTML, any other `<!` (a bogus comment).
    fn declaration(&mut self) -> Result<(), Refusal> {
        let at = self.pos;
        let bytes = self.bytes;
        let rest = &bytes[at..];
        if rest.starts_with(b"<!--") {
            let Some(end) = comment_end(bytes, at, self.html) else {
                return Err(Refusal::at(RefusalReason::UnterminatedComment, at));
            };
            return self.leaf(NodeKind::Comment, at, end);
        }
        if rest.starts_with(b"<![CDATA[") {
            if self.html && self.foreign == 0 {
                return self.bogus_comment(at);
            }
            let Some(close) = find(bytes, at + 9, b"]]>") else {
                return Err(Refusal::at(RefusalReason::UnterminatedConstruct, at));
            };
            return self.leaf(NodeKind::Cdata, at, close + 3);
        }
        if starts_with_ci(rest, b"<!DOCTYPE") {
            if self.html {
                let Some(close) = find(bytes, at + 9, b">") else {
                    return Err(Refusal::at(RefusalReason::UnterminatedConstruct, at));
                };
                return self.leaf(NodeKind::Doctype, at, close + 1);
            }
            if !self.open.is_empty() {
                return Err(Refusal::at(RefusalReason::UnsupportedSyntax, at));
            }
            let Some((end, subset)) = xml_doctype_end(bytes, at + 9) else {
                return Err(Refusal::at(RefusalReason::UnterminatedConstruct, at));
            };
            self.internal_subset |= subset;
            return self.leaf(NodeKind::Doctype, at, end);
        }
        if self.html {
            self.bogus_comment(at)
        } else {
            Err(Refusal::at(RefusalReason::UnsupportedSyntax, at))
        }
    }

    /// An HTML bogus comment: everything up to the first `>`.
    fn bogus_comment(&mut self, at: usize) -> Result<(), Refusal> {
        let Some(close) = find(self.bytes, at + 2, b">") else {
            return Err(Refusal::at(RefusalReason::UnterminatedConstruct, at));
        };
        self.leaf(NodeKind::Comment, at, close + 1)
    }

    /// `<?…?>`: a processing instruction in XML, a bogus comment in HTML.
    fn instruction(&mut self) -> Result<(), Refusal> {
        let at = self.pos;
        if self.html {
            return self.bogus_comment(at);
        }
        let Some(close) = find(self.bytes, at + 2, b"?>") else {
            return Err(Refusal::at(RefusalReason::UnterminatedConstruct, at));
        };
        self.leaf(NodeKind::Pi, at, close + 2)
    }

    fn end_tag(&mut self) -> Result<(), Refusal> {
        let at = self.pos;
        let bytes = self.bytes;
        let name_start = at + 2;
        if self.html && !bytes.get(name_start).is_some_and(u8::is_ascii_alphabetic) {
            return self.bogus_comment(at);
        }
        let name_end = self.scan_name(name_start);
        if name_end == name_start {
            return Err(Refusal::at(RefusalReason::MalformedTag, at));
        }
        let tag = self.scan_tag_end(name_end)?;
        if tag.self_closing && !self.html {
            return Err(Refusal::at(RefusalReason::MalformedTag, at));
        }
        let Some(open) = self.open.last().copied() else {
            return Err(Refusal::at(RefusalReason::MismatchedTag, at));
        };
        let name = &bytes[name_start..name_end];
        let expected = &bytes[open.name_start..open.name_end];
        let same = if self.html {
            name.eq_ignore_ascii_case(expected)
        } else {
            name == expected
        };
        if !same {
            return Err(Refusal::at(RefusalReason::MismatchedTag, at));
        }
        let node = &mut self.nodes[open.node];
        node.close_start = at;
        node.end = tag.end;
        self.open.pop();
        if open.foreign_root {
            self.foreign -= 1;
        }
        self.pos = tag.end;
        Ok(())
    }

    fn start_tag(&mut self) -> Result<(), Refusal> {
        let at = self.pos;
        let bytes = self.bytes;
        let name_start = at + 1;
        let name_end = self.scan_name(name_start);
        if name_end == name_start {
            return Err(Refusal::at(RefusalReason::MalformedTag, at));
        }
        let tag = self.scan_tag_end(name_end)?;
        let name = &bytes[name_start..name_end];
        let html = self.html;
        let foreign_root =
            html && (name.eq_ignore_ascii_case(b"svg") || name.eq_ignore_ascii_case(b"math"));
        let foreign = html && (self.foreign > 0 || foreign_root);
        let void = html && !foreign && name_in(name, VOID);
        // HTML ignores the slash of `<div/>`; only void and foreign elements close with it.
        let closed = void || (tag.self_closing && (!html || foreign));
        let raw_text = html && !foreign && !closed && name_in(name, RAW_TEXT);
        if self.open.len() >= MAX_DEPTH {
            return Err(Refusal::at(RefusalReason::NestingTooDeep, at));
        }
        let mut node = Node::leaf(NodeKind::Element, at, tag.end);
        node.verbatim = tag.preserve
            || tag.white_space
            || tag.display
            || raw_text
            || keeps_whitespace(name, html);
        // `white-space` changes how the content is laid out, not what the element is to its
        // neighbours; `display` may turn a block into an inline element, so the parent must
        // not put line breaks between it and its siblings.
        node.block = !html || (name_in(name, BLOCK) && !tag.display);
        let index = self.append(node);
        if closed {
            self.pos = tag.end;
            return Ok(());
        }
        if raw_text {
            let Some(close) = find_raw_end(bytes, tag.end, name) else {
                return Err(Refusal::at(RefusalReason::UnclosedTag, at));
            };
            if name.eq_ignore_ascii_case(b"script") && double_escaped(&bytes[tag.end..close]) {
                return Err(Refusal::at(RefusalReason::UnsupportedSyntax, at));
            }
            let end = self.scan_tag_end(close + 2 + name.len())?.end;
            self.nodes[index].close_start = close;
            self.nodes[index].end = end;
            self.pos = end;
            return Ok(());
        }
        self.open.push(Open {
            node: index,
            name_start,
            name_end,
            foreign_root,
        });
        if foreign_root {
            self.foreign += 1;
        }
        self.pos = tag.end;
        Ok(())
    }

    /// End of the tag or element name that starts at `from`. HTML names run up to whitespace,
    /// `/` or `>`; XML names consist of name characters.
    fn scan_name(&self, from: usize) -> usize {
        let bytes = self.bytes;
        let mut i = from;
        if self.html {
            while let Some(&byte) = bytes.get(i) {
                if is_space(byte) || byte == b'/' || byte == b'>' {
                    break;
                }
                i += 1;
            }
            return i;
        }
        while let Some(&byte) = bytes.get(i) {
            let ok = if i == from {
                byte.is_ascii_alphabetic() || matches!(byte, b'_' | b':') || byte >= 0x80
            } else {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'_' | b':' | b'-' | b'.')
                    || byte >= 0x80
            };
            if !ok {
                break;
            }
            i += 1;
        }
        i
    }

    /// End of the attribute name that starts at `from` (HTML: up to whitespace, `/`, `>` or
    /// `=`, the first character always belongs to it).
    fn scan_attribute_name(&self, from: usize) -> usize {
        if !self.html {
            return self.scan_name(from);
        }
        let mut i = from + 1;
        while let Some(&byte) = self.bytes.get(i) {
            if is_space(byte) || matches!(byte, b'/' | b'>' | b'=') {
                break;
            }
            i += 1;
        }
        i
    }

    /// Scans the attributes of the tag whose name ends at `from`, up to and including `>`.
    ///
    /// Attribute values are skipped, never interpreted, except for the two that make an element
    /// verbatim. XML needs quoted values and whitespace between attributes; HTML takes
    /// unquoted values and stray slashes the way the HTML tokenizer does.
    fn scan_tag_end(&self, from: usize) -> Result<TagEnd, Refusal> {
        let bytes = self.bytes;
        let mut tag = TagEnd {
            end: 0,
            self_closing: false,
            preserve: false,
            white_space: false,
            display: false,
        };
        let mut i = from;
        loop {
            let before = i;
            while i < bytes.len() && is_space(bytes[i]) {
                i += 1;
            }
            let spaced = i > before;
            let Some(&byte) = bytes.get(i) else {
                return Err(Refusal::at(RefusalReason::UnexpectedEnd, bytes.len()));
            };
            if byte == b'>' {
                tag.end = i + 1;
                return Ok(tag);
            }
            if byte == b'/' {
                if bytes.get(i + 1) == Some(&b'>') {
                    tag.end = i + 2;
                    tag.self_closing = true;
                    return Ok(tag);
                }
                if self.html {
                    i += 1;
                    continue;
                }
                return Err(Refusal::at(RefusalReason::MalformedTag, i));
            }
            if !self.html && !spaced {
                return Err(Refusal::at(RefusalReason::MalformedTag, i));
            }
            let name_start = i;
            let name_end = self.scan_attribute_name(name_start);
            if name_end == name_start {
                return Err(Refusal::at(RefusalReason::MalformedTag, name_start));
            }
            i = name_end;
            while i < bytes.len() && is_space(bytes[i]) {
                i += 1;
            }
            let mut value = (i, i);
            if bytes.get(i) == Some(&b'=') {
                i += 1;
                while i < bytes.len() && is_space(bytes[i]) {
                    i += 1;
                }
                match bytes.get(i).copied() {
                    Some(quote @ (b'"' | b'\'')) => {
                        let rest = &bytes[i + 1..];
                        // A raw `<` cannot occur in an XML attribute value: a quote that is
                        // not closed before one is an unclosed quote.
                        let closing = rest
                            .iter()
                            .position(|&byte| byte == quote || (!self.html && byte == b'<'));
                        let Some(length) = closing.filter(|&length| rest[length] == quote) else {
                            return Err(Refusal::at(RefusalReason::UnterminatedAttribute, i));
                        };
                        value = (i + 1, i + 1 + length);
                        i += length + 2;
                    }
                    Some(_) if self.html => {
                        let begin = i;
                        while i < bytes.len() && !is_space(bytes[i]) && bytes[i] != b'>' {
                            i += 1;
                        }
                        value = (begin, i);
                    }
                    Some(_) => return Err(Refusal::at(RefusalReason::MalformedTag, i)),
                    None => return Err(Refusal::at(RefusalReason::UnexpectedEnd, bytes.len())),
                }
            } else if !self.html {
                return Err(Refusal::at(RefusalReason::MalformedTag, i));
            }
            let name = &bytes[name_start..name_end];
            let value = &bytes[value.0..value.1];
            if name.eq_ignore_ascii_case(b"xml:space") && value.eq_ignore_ascii_case(b"preserve") {
                tag.preserve = true;
            }
            if name.eq_ignore_ascii_case(b"style") {
                tag.white_space |= contains_ci(value, b"white-space");
                tag.display |= contains_ci(value, b"display");
            }
        }
    }
}

/// End of the comment that starts at `at` (`<!--`), or `None` when it never ends. HTML also
/// ends a comment at `--!>` and treats `<!-->` and `<!--->` as empty comments.
fn comment_end(bytes: &[u8], at: usize, html: bool) -> Option<usize> {
    let from = at + 4;
    if html {
        if bytes.get(from) == Some(&b'>') {
            return Some(from + 1);
        }
        if bytes.get(from) == Some(&b'-') && bytes.get(from + 1) == Some(&b'>') {
            return Some(from + 2);
        }
    }
    let mut i = from;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-' && bytes[i + 1] == b'-' {
            match bytes.get(i + 2) {
                Some(b'>') => return Some(i + 3),
                Some(b'!') if html && bytes.get(i + 3) == Some(&b'>') => return Some(i + 4),
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// End of the XML DOCTYPE whose keyword ends at `from`, and whether it has an internal subset.
/// Quoted literals and comments inside the subset may contain `>` and brackets.
fn xml_doctype_end(bytes: &[u8], from: usize) -> Option<(usize, bool)> {
    let mut i = from;
    let mut depth = 0usize;
    let mut subset = false;
    let mut quote = 0u8;
    while i < bytes.len() {
        let byte = bytes[i];
        if quote != 0 {
            if byte == quote {
                quote = 0;
            }
            i += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = byte,
            b'[' => {
                depth += 1;
                subset = true;
            }
            b']' => depth = depth.saturating_sub(1),
            b'<' if depth > 0 && bytes[i..].starts_with(b"<!--") => {
                i = find(bytes, i + 4, b"-->")? + 3;
                continue;
            }
            b'>' if depth == 0 => return Some((i + 1, subset)),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Start of the end tag of the raw-text element `name` whose content starts at `from`: `</`,
/// the name in any case, then whitespace, `/` or `>`.
fn find_raw_end(bytes: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut i = from;
    while let Some(at) = find(bytes, i, b"</") {
        let name_at = at + 2;
        let named = bytes
            .get(name_at..name_at + name.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name));
        if named
            && matches!(
                bytes.get(name_at + name.len()),
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0c | b'/' | b'>')
            )
        {
            return Some(at);
        }
        i = at + 2;
    }
    None
}

/// Whether script text contains `<!--` followed by `<script`: the HTML tokenizer's
/// double-escaped state, where `</script>` does not end the script.
fn double_escaped(content: &[u8]) -> bool {
    find(content, 0, b"<!--").is_some_and(|at| contains_ci(&content[at..], b"<script"))
}

// ---------------------------------------------------------------------------------- printer

#[derive(Clone, Copy, Debug)]
enum Piece {
    /// A range of the source, copied verbatim.
    Raw(usize, usize),
    Newline,
    /// This many indent units.
    Indent(usize),
}

struct Planner<'a> {
    bytes: &'a [u8],
    nodes: &'a [Node],
    html: bool,
    pieces: Vec<Piece>,
    /// The line breaks of the whitespace the formatter replaces, by spelling.
    gaps: EolCounts,
}

impl Planner<'_> {
    fn raw(&mut self, start: usize, end: usize) {
        if start < end {
            self.pieces.push(Piece::Raw(start, end));
        }
    }

    fn indent(&mut self, depth: usize) {
        if depth > 0 {
            self.pieces.push(Piece::Indent(depth));
        }
    }

    /// Whether the children of `index` can each go on a line of their own.
    fn expandable(&self, index: usize) -> bool {
        let nodes = self.nodes;
        let node = &nodes[index];
        let is_document = node.kind == NodeKind::Document;
        if node.verbatim || !node.block {
            return false;
        }
        let mut content = false;
        for child in children(nodes, index) {
            let child = &nodes[child];
            match child.kind {
                NodeKind::Blank => {
                    // Outside the root element XML whitespace is not content; inside, a gap
                    // without a line break (`<a/> <b/>`) is data.
                    if !self.html && !is_document && !has_break(&self.bytes[child.start..child.end])
                    {
                        return false;
                    }
                }
                NodeKind::Comment | NodeKind::Pi | NodeKind::Doctype => content = true,
                NodeKind::Element => {
                    if self.html && !child.block {
                        return false;
                    }
                    content = true;
                }
                NodeKind::Text | NodeKind::Cdata | NodeKind::Document => return false,
            }
        }
        content
    }

    /// Plans one non-blank node at `depth`.
    fn plan_node(&mut self, index: usize, depth: usize) {
        let node = self.nodes[index];
        if node.kind == NodeKind::Element && self.expandable(index) {
            self.raw(node.start, node.open_end);
            self.plan_children(index, depth + 1);
            self.pieces.push(Piece::Newline);
            self.indent(depth);
            self.raw(node.close_start, node.end);
        } else {
            self.raw(node.start, node.end);
        }
    }

    /// Plans the children of the expandable node `index`, each on its own line at `depth`.
    /// Between two children one blank line survives; none at the start or the end of an
    /// element. The top level starts at the first node and ends in a line break exactly when
    /// the source did.
    fn plan_children(&mut self, index: usize, depth: usize) {
        let nodes = self.nodes;
        let is_document = nodes[index].kind == NodeKind::Document;
        let mut breaks = 0;
        let mut first = true;
        for child in children(nodes, index) {
            let node = &nodes[child];
            if node.kind == NodeKind::Blank {
                breaks = self.gaps.count(&self.bytes[node.start..node.end]);
                continue;
            }
            if !(first && is_document) {
                self.pieces.push(Piece::Newline);
                if breaks >= 2 && !first {
                    self.pieces.push(Piece::Newline);
                }
                self.indent(depth);
            }
            self.plan_node(child, depth);
            first = false;
            breaks = 0;
        }
        if is_document && breaks > 0 {
            self.pieces.push(Piece::Newline);
        }
    }
}

fn has_break(bytes: &[u8]) -> bool {
    bytes.iter().any(|&byte| byte == b'\n' || byte == b'\r')
}

fn render(text: &str, pieces: &[Piece], eol: &str, unit: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for piece in pieces {
        match *piece {
            Piece::Raw(start, end) => out.push_str(&text[start..end]),
            Piece::Newline => out.push_str(eol),
            Piece::Indent(depth) => {
                for _ in 0..depth {
                    out.push_str(unit);
                }
            }
        }
    }
    out
}
