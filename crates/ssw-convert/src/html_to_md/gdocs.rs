//! Google Docs clipboard HTML.
//!
//! Docs wraps the selection in `<b style="font-weight:normal" id="docs-internal-guid-…">` (the
//! explicit `font-weight` keeps that wrapper from reading as bold), writes every run as a
//! `<span>` with inline `font-weight:700` / `font-style:italic` / `text-decoration` declarations,
//! and lists as `<ul>`/`<ol>` whose items carry `aria-level` (nested lists sit beside the items
//! inside the parent list element, which is invalid HTML but kept as parsed).

use markup5ever_rcdom::Handle;

use super::dom::{Style, attr, children, tag};

/// Whether the document comes from Google Docs.
pub(super) fn detect(root: &Handle) -> bool {
    let mut stack = vec![root.clone()];
    while let Some(node) = stack.pop() {
        if attr(&node, "id").is_some_and(|id| id.starts_with("docs-internal-guid-")) {
            return true;
        }
        stack.extend(children(&node));
    }
    false
}

/// One-based nesting level of a list item.
pub(super) fn aria_level(item: &Handle) -> Option<usize> {
    attr(item, "aria-level")?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|level| *level >= 1)
}

/// Whether the list's items declare their own nesting level.
pub(super) fn has_levels(list: &Handle) -> bool {
    children(list)
        .iter()
        .any(|c| tag(c) == Some("li") && aria_level(c).is_some())
}

/// Docs marks highlighted text with a `background-color`; every other run says `transparent`.
pub(super) fn is_highlight(style: &Style) -> bool {
    style.get("background-color").is_some_and(|v| {
        !matches!(
            v,
            "transparent"
                | "initial"
                | "inherit"
                | "none"
                | "white"
                | "#fff"
                | "#ffffff"
                | "rgb(255, 255, 255)"
                | "rgba(0, 0, 0, 0)"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html_to_md::dom::parse;

    #[test]
    fn detects_the_docs_wrapper_anywhere() {
        assert!(detect(&parse(
            "<b style='font-weight:normal' id='docs-internal-guid-1-2'><p>x</p></b>"
        )));
        assert!(detect(&parse(
            "<div><p><span id='docs-internal-guid-9'>x</span></p></div>"
        )));
        assert!(!detect(&parse(
            "<b id='guid'>x</b><p id=docs-internal>y</p><p>z</p>"
        )));
    }

    #[test]
    fn list_levels() {
        let doc = parse(
            "<ul id=a><li aria-level=2>x</li></ul><ul id=b><li aria-level=0>x</li><li>y</li></ul>\
             <ul id=c><li aria-level=x>x</li></ul><ol id=d><p aria-level=1>x</p></ol>",
        );
        let list = |id: &str| {
            let mut stack = vec![doc.clone()];
            while let Some(node) = stack.pop() {
                if attr(&node, "id").as_deref() == Some(id) {
                    return node;
                }
                stack.extend(children(&node));
            }
            panic!("no list {id}");
        };
        let first_item = |id: &str| {
            children(&list(id))
                .into_iter()
                .find(|c| tag(c) == Some("li"))
        };
        assert_eq!(aria_level(&first_item("a").unwrap()), Some(2));
        assert_eq!(aria_level(&first_item("b").unwrap()), None);
        assert_eq!(aria_level(&first_item("c").unwrap()), None);
        let expected = [("a", true), ("b", false), ("c", false), ("d", false)];
        for (id, has) in expected {
            assert_eq!(has_levels(&list(id)), has, "{id}");
        }
    }

    #[test]
    fn highlight_table() {
        let table = [
            ("background-color:#ffff00", true),
            ("background-color: rgb(255, 255, 0)", true),
            ("background-color:transparent", false),
            ("background-color:#ffffff", false),
            ("background-color:#FFF", false),
            ("background-color:white", false),
            ("background-color:rgba(0, 0, 0, 0)", false),
            ("background-color:inherit", false),
            ("color:red", false),
        ];
        for (css, expected) in table {
            assert_eq!(is_highlight(&Style::parse(css)), expected, "{css}");
        }
    }
}
