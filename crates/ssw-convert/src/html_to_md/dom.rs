//! Thin helpers over the `markup5ever_rcdom` tree: parsing, tag/attribute access, inline CSS and
//! text extraction.

use html5ever::tendril::TendrilSink;
use html5ever::{ParseOpts, parse_document};
use markup5ever_rcdom::{Handle, NodeData, RcDom};

/// Parses `html` as a complete document (fragments end up inside `<body>`).
pub(super) fn parse(html: &str) -> Handle {
    parse_document(RcDom::default(), ParseOpts::default())
        .one(html)
        .document
}

/// Lower-case local name of an element node.
pub(super) fn tag(node: &Handle) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(&*name.local),
        _ => None,
    }
}

/// Value of the attribute `name` (attribute names are lower-case after parsing).
pub(super) fn attr(node: &Handle, name: &str) -> Option<String> {
    let NodeData::Element { attrs, .. } = &node.data else {
        return None;
    };
    attrs
        .borrow()
        .iter()
        .find(|a| &*a.name.local == name)
        .map(|a| a.value.to_string())
}

/// Contents of a text node.
pub(super) fn text_of(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Text { contents } => Some(contents.borrow().to_string()),
        _ => None,
    }
}

/// Snapshot of the children of a node (cheap: reference-counted handles).
pub(super) fn children(node: &Handle) -> Vec<Handle> {
    node.children.borrow().clone()
}

/// Whether the `class` attribute contains `class` (ASCII case-insensitive).
pub(super) fn has_class(node: &Handle, class: &str) -> bool {
    attr(node, "class").is_some_and(|c| c.split_whitespace().any(|t| t.eq_ignore_ascii_case(class)))
}

/// All text below `node` in document order, without descending into `script`/`style`.
pub(super) fn text_content(node: &Handle) -> String {
    let mut out = String::new();
    let mut stack = vec![node.clone()];
    while let Some(current) = stack.pop() {
        match &current.data {
            NodeData::Text { contents } => out.push_str(&contents.borrow()),
            NodeData::Element { .. } if matches!(tag(&current), Some("script" | "style")) => {}
            _ => stack.extend(current.children.borrow().iter().rev().cloned()),
        }
    }
    out
}

/// Parsed inline `style` declarations; property names and values are lower-cased.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Style(Vec<(String, String)>);

impl Style {
    /// The `style` attribute of `node` (empty when absent).
    pub(super) fn of(node: &Handle) -> Style {
        attr(node, "style")
            .map(|s| Style::parse(&s))
            .unwrap_or_default()
    }

    /// Parses a declaration list. Semicolons inside quotes or parentheses do not split.
    pub(super) fn parse(css: &str) -> Style {
        let mut declarations = Vec::new();
        let mut current = String::new();
        let mut quote: Option<char> = None;
        let mut parens = 0usize;
        for c in css.chars() {
            match (quote, c) {
                (Some(q), _) if c == q => quote = None,
                (None, '"' | '\'') => quote = Some(c),
                (None, '(') => parens += 1,
                (None, ')') => parens = parens.saturating_sub(1),
                (None, ';') if parens == 0 => {
                    declarations.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            }
            current.push(c);
        }
        declarations.push(current);
        let parsed = declarations
            .iter()
            .filter_map(|d| {
                let (name, value) = d.split_once(':')?;
                let name = name.trim().to_lowercase();
                let value = value.trim().to_lowercase();
                let value = value
                    .strip_suffix("!important")
                    .map_or(value.as_str(), str::trim_end);
                (!name.is_empty()).then(|| (name, value.to_string()))
            })
            .collect();
        Style(parsed)
    }

    /// The value of `property`; the last declaration wins.
    pub(super) fn get(&self, property: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == property)
            .map(|(_, v)| v.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first `<name>` below `document`. The document must stay alive while the result is
    /// used: dropping the root empties the children of every node below it.
    fn first_element(document: &Handle, name: &str) -> Handle {
        let mut stack = vec![document.clone()];
        let mut found = None;
        while let Some(node) = stack.pop() {
            if tag(&node) == Some(name) {
                found = Some(node);
                break;
            }
            stack.extend(children(&node).into_iter().rev());
        }
        found.expect("the document has the element")
    }

    #[test]
    fn style_parsing_table() {
        let table = [
            ("", vec![]),
            ("color:red", vec![("color", "red")]),
            (
                " Color : Red ; ; font-weight:700 !important;",
                vec![("color", "red"), ("font-weight", "700")],
            ),
            (
                "font-family: \"Courier; New\", monospace; x:y",
                vec![("font-family", "\"courier; new\", monospace"), ("x", "y")],
            ),
            (
                "background:url(data:image/png;base64,AA); a:b",
                vec![("background", "url(data:image/png;base64,aa)"), ("a", "b")],
            ),
            ("no-colon; :empty; ok:1", vec![("ok", "1")]),
            ("a:b)); c:d", vec![("a", "b))"), ("c", "d")]),
        ];
        for (css, expected) in table {
            let expected: Vec<(String, String)> = expected
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            assert_eq!(Style::parse(css).0, expected, "{css}");
        }
    }

    #[test]
    fn last_declaration_wins() {
        let style = Style::parse("white-space:pre;white-space:pre-wrap;color:red");
        assert_eq!(style.get("white-space"), Some("pre-wrap"));
        assert_eq!(style.get("color"), Some("red"));
        assert_eq!(style.get("margin"), None);
    }

    #[test]
    fn element_access() {
        let document = parse("<p id=a class='Foo bar' style='X:Y'>t</p>");
        let p = first_element(&document, "p");
        assert_eq!(tag(&p), Some("p"));
        assert_eq!(attr(&p, "id").as_deref(), Some("a"));
        assert_eq!(attr(&p, "missing"), None);
        assert!(has_class(&p, "foo") && has_class(&p, "BAR") && !has_class(&p, "baz"));
        assert_eq!(Style::of(&p).get("x"), Some("y"));
        let text = children(&p)[0].clone();
        assert_eq!(text_of(&text).as_deref(), Some("t"));
        assert_eq!(text_of(&p), None);
        assert_eq!(tag(&text), None);
        assert_eq!(attr(&text, "id"), None);
        assert_eq!(Style::of(&text), Style::default());
        assert!(!has_class(&text, "foo"));
    }

    #[test]
    fn text_content_skips_scripts_and_keeps_order() {
        let document =
            parse("<div>a<b>b</b><script>no</script><style>no</style>c<i>d<u>e</u></i></div>");
        let div = first_element(&document, "div");
        assert_eq!(text_content(&div), "abcde");
    }
}
