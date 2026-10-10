//! A small XML tree for ODF parts.
//!
//! Element and attribute names keep the prefix as written (`text:p`, `fo:font-weight`): every
//! ODF producer uses the standard prefixes, and matching on them keeps the readers short.
//! Namespace declarations are dropped.

use quick_xml::Reader;
use quick_xml::escape::{resolve_predefined_entity, unescape};
use quick_xml::events::{BytesStart, Event};

/// Deepest element nesting accepted, so recursive consumers cannot overflow the stack.
pub(super) const MAX_DEPTH: usize = 128;

/// A child of an [`Element`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Node {
    Element(Element),
    Text(String),
}

/// An XML element.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct Element {
    /// Qualified name as written, e.g. `text:p`.
    pub name: String,
    /// Attributes in document order with qualified names, e.g. `text:style-name`.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

/// Why a part could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum XmlError {
    #[error("malformed XML: {0}")]
    Malformed(String),
    #[error("XML nesting is deeper than {MAX_DEPTH} levels")]
    TooDeep,
    #[error("the part has no root element")]
    Empty,
}

impl Element {
    /// Value of the attribute with the qualified name `name`.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Child elements in document order.
    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    /// First child element with the given name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.elements().find(|e| e.name == name)
    }

    /// All text below this element in document order.
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

fn element_of(start: &BytesStart<'_>) -> Element {
    let name = start.name().as_ref().to_string();
    let mut attrs = Vec::new();
    for attr in start.attributes().flatten() {
        let key = attr.key.as_ref().to_string();
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        let value =
            unescape(&attr.value).map_or_else(|_| attr.value.to_string(), |c| c.into_owned());
        attrs.push((key, value));
    }
    Element {
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

fn attach(
    stack: &mut [Element],
    root: &mut Option<Element>,
    element: Element,
) -> Result<(), XmlError> {
    match stack.last_mut() {
        Some(parent) => parent.children.push(Node::Element(element)),
        None => {
            if root.is_some() {
                return Err(XmlError::Malformed("more than one root element".into()));
            }
            *root = Some(element);
        }
    }
    Ok(())
}

/// Parses one XML part. A leading UTF-8 byte order mark is accepted; entities other than the
/// predefined ones and character references are dropped.
pub(super) fn parse(bytes: &[u8]) -> Result<Element, XmlError> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    let mut reader = Reader::from_reader(bytes);
    let mut buf = Vec::new();
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    loop {
        buf.clear();
        let event = reader
            .read_event_into(&mut buf)
            .map_err(|e| XmlError::Malformed(e.to_string()))?;
        match event {
            Event::Start(start) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(XmlError::TooDeep);
                }
                stack.push(element_of(&start));
            }
            Event::Empty(start) => attach(&mut stack, &mut root, element_of(&start))?,
            Event::End(_) => {
                // The reader refuses an end tag that closes nothing, so an element is open.
                stack
                    .pop()
                    .map(|element| attach(&mut stack, &mut root, element))
                    .transpose()?;
            }
            Event::Text(text) => push_text(&mut stack, &text.xml10_content()),
            Event::CData(data) => push_text(&mut stack, &data.xml10_content()),
            Event::GeneralRef(reference) => match reference.resolve_char_ref() {
                Ok(Some(c)) => push_text(&mut stack, c.encode_utf8(&mut [0u8; 4])),
                Ok(None) => {
                    if let Some(s) = resolve_predefined_entity(&reference.xml10_content()) {
                        push_text(&mut stack, s);
                    }
                }
                Err(_) => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(XmlError::Malformed("unclosed element".into()));
    }
    root.ok_or(XmlError::Empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cdata_adds_no_text_node() {
        let root = parse(b"<a>x<![CDATA[]]>y</a>").unwrap();
        assert_eq!(root.text(), "xy");
        assert_eq!(root.children.len(), 1, "the text on both sides is one node");
    }

    #[test]
    fn builds_a_tree_with_qualified_names_and_attributes() {
        let xml = r#"<?xml version="1.0"?><office:text xmlns:office="urn:x" xmlns="urn:d" a="1"><text:p text:style-name="P&amp;1" fo:x='q"'>Hello <text:span>big</text:span> world</text:p><text:p/></office:text>"#;
        let root = parse(xml.as_bytes()).unwrap();
        assert_eq!(root.name, "office:text");
        assert_eq!(
            root.attrs,
            [("a".to_string(), "1".to_string())],
            "xmlns declarations are dropped"
        );
        let paragraphs: Vec<&Element> = root.elements().collect();
        assert_eq!(paragraphs.len(), 2);
        assert_eq!(paragraphs[0].attr("text:style-name"), Some("P&1"));
        assert_eq!(paragraphs[0].attr("fo:x"), Some("q\""));
        assert_eq!(paragraphs[0].attr("missing"), None);
        assert_eq!(paragraphs[0].text(), "Hello big world");
        assert_eq!(
            paragraphs[0].child("text:span").map(Element::text),
            Some("big".to_string())
        );
        assert!(paragraphs[0].child("text:s").is_none());
        assert!(paragraphs[1].children.is_empty());
        assert_eq!(root.text(), "Hello big world");
    }

    #[test]
    fn decodes_references_cdata_and_bom() {
        let xml = "\u{feff}<r>a &lt;&#x42;&#67;&unknown; <![CDATA[<d>]]>\r\nz</r>";
        let root = parse(xml.as_bytes()).unwrap();
        assert_eq!(root.text(), "a <BC <d>\nz");
        assert_eq!(root.children.len(), 1, "adjacent text is merged");
        let bad_ref = parse(b"<r>x&#0;y</r>").unwrap();
        assert_eq!(bad_ref.text(), "xy");
        let bad_attr = parse(b"<r a=\"&bogus;\"/>").unwrap();
        assert_eq!(bad_attr.attr("a"), Some("&bogus;"));
    }

    #[test]
    fn rejects_broken_input() {
        for broken in [&b"<a><b></a>"[..], b"</a>", b"<a><b>"] {
            let message = parse(broken).unwrap_err().to_string();
            assert!(
                message.starts_with("malformed XML: "),
                "{broken:?}: {message}"
            );
        }
        let second_root = XmlError::Malformed("more than one root element".into());
        assert_eq!(parse(b"<a/><b/>"), Err(second_root.clone()));
        assert_eq!(parse(b"<a></a><b></b>"), Err(second_root));
        assert_eq!(parse(b"  "), Err(XmlError::Empty));
        assert_eq!(parse(b""), Err(XmlError::Empty));
        assert_eq!(
            parse(b"<?xml version=\"1.0\"?><!-- c -->"),
            Err(XmlError::Empty)
        );
    }

    #[test]
    fn nesting_is_bounded() {
        let ok = format!("{}x{}", "<a>".repeat(MAX_DEPTH), "</a>".repeat(MAX_DEPTH));
        assert!(parse(ok.as_bytes()).is_ok());
        let deep = format!(
            "{}x{}",
            "<a>".repeat(MAX_DEPTH + 1),
            "</a>".repeat(MAX_DEPTH + 1)
        );
        assert_eq!(parse(deep.as_bytes()), Err(XmlError::TooDeep));
    }
}
