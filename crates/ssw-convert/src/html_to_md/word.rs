//! Microsoft Word clipboard HTML: list paragraphs (`mso-list`), bullet/number marker text,
//! Title/Heading paragraph classes and highlighting.
//!
//! Word writes lists as flat paragraphs: `<p class=MsoListParagraph style='mso-list:l1 level2
//! lfo1'>` whose first child is a `mso-list:Ignore` span holding the visible marker (`1.`,
//! `a)`, a Symbol bullet, ...). The paragraphs are regrouped into nested lists by the caller.

use markup5ever_rcdom::Handle;

use super::dom::{Style, children, text_content};

/// The `mso-list` property of a list paragraph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListInfo {
    /// List instance (`l1 lfo1`): paragraphs of different instances never share a list.
    pub(super) key: String,
    /// Zero-based nesting level.
    pub(super) level: usize,
}

/// Reads `mso-list:l1 level2 lfo1`; `None` for `mso-list:none`/`Ignore` or malformed values.
pub(super) fn list_info(style: &Style) -> Option<ListInfo> {
    let value = style.get("mso-list")?;
    let mut tokens = value.split_whitespace();
    let id = tokens
        .next()
        .filter(|t| t.strip_prefix('l').is_some_and(is_number))?;
    let mut level = 1;
    let mut lfo = "";
    for token in tokens {
        if let Some(n) = token
            .strip_prefix("level")
            .and_then(|n| n.parse::<usize>().ok())
        {
            level = n.clamp(1, 9);
        } else if token.starts_with("lfo") {
            lfo = token;
        }
    }
    Some(ListInfo {
        key: format!("{id} {lfo}"),
        level: level - 1,
    })
}

fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Whether the element carries Word's `mso-list:Ignore` marker (the visible list label).
pub(super) fn is_marker(style: &Style) -> bool {
    style.get("mso-list") == Some("ignore")
}

/// Text of the first marker element below `paragraph`, trimmed.
pub(super) fn find_marker(paragraph: &Handle) -> Option<String> {
    let mut stack = children(paragraph);
    stack.reverse();
    while let Some(node) = stack.pop() {
        if is_marker(&Style::of(&node)) {
            return Some(text_content(&node).trim().to_string());
        }
        let mut kids = children(&node);
        kids.reverse();
        stack.extend(kids);
    }
    None
}

/// What a list marker says about the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Marker {
    pub(super) ordered: bool,
    /// `1)` instead of `1.`.
    pub(super) paren: bool,
    /// Number of the item (1 for alphabetic and roman markers).
    pub(super) start: u64,
    /// Letters or roman numerals: Markdown can only number with digits.
    pub(super) lettered: bool,
}

/// Interprets marker text: a bullet glyph (no trailing delimiter) or `1.`/`1)`/`(1)`/`a.`/`iv)`.
pub(super) fn parse_marker(text: &str) -> Marker {
    let bullet = Marker {
        ordered: false,
        paren: false,
        start: 1,
        lettered: false,
    };
    let (stem, paren) = if let Some(stem) = text.strip_suffix('.') {
        (stem, false)
    } else if let Some(stem) = text.strip_suffix(')') {
        (stem.strip_prefix('(').unwrap_or(stem), true)
    } else {
        return bullet;
    };
    // `1.2.3` is numbered by its last component.
    let last = stem.rsplit('.').next().unwrap_or(stem);
    if is_number(last) {
        return Marker {
            ordered: true,
            paren,
            start: last.parse().unwrap_or(1),
            lettered: false,
        };
    }
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_alphabetic()) {
        return Marker {
            ordered: true,
            paren,
            start: 1,
            lettered: true,
        };
    }
    bullet
}

/// Heading level for the Word paragraph classes `MsoTitle`, `MsoSubtitle` and `MsoHeading1`..`9`.
pub(super) fn heading_level(class: &str) -> Option<u8> {
    class.split_whitespace().find_map(|token| {
        let lower = token.to_ascii_lowercase();
        match lower.as_str() {
            "msotitle" => Some(1),
            "msosubtitle" => Some(2),
            _ => lower
                .strip_prefix("msoheading")?
                .parse::<u8>()
                .ok()
                .filter(|n| *n >= 1)
                .map(|n| n.min(6)),
        }
    })
}

/// Word text highlighting: `mso-highlight:yellow`.
pub(super) fn is_highlight(style: &Style) -> bool {
    style
        .get("mso-highlight")
        .is_some_and(|v| !matches!(v, "auto" | "none" | "windowtext"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html_to_md::dom::{parse, tag};

    #[test]
    fn list_info_table() {
        let table = [
            ("mso-list:l1 level2 lfo1", Some(("l1 lfo1", 1))),
            ("mso-list: l12 level1 lfo30", Some(("l12 lfo30", 0))),
            ("mso-list:l0 level9 lfo1", Some(("l0 lfo1", 8))),
            ("mso-list:l0 level99 lfo1", Some(("l0 lfo1", 8))),
            ("mso-list:l0 level0 lfo1", Some(("l0 lfo1", 0))),
            ("mso-list:l3", Some(("l3 ", 0))),
            ("mso-list:l3 levelx other", Some(("l3 ", 0))),
            ("mso-list:none", None),
            ("mso-list:Ignore", None),
            ("mso-list:l level1", None),
            ("mso-list:", None),
            ("color:red", None),
        ];
        for (css, expected) in table {
            let expected = expected.map(|(key, level)| ListInfo {
                key: key.to_string(),
                level,
            });
            assert_eq!(list_info(&Style::parse(css)), expected, "{css}");
        }
    }

    #[test]
    fn marker_detection() {
        assert!(is_marker(&Style::parse("mso-list:Ignore")));
        assert!(!is_marker(&Style::parse("mso-list:l1 level1 lfo1")));
        assert!(!is_marker(&Style::parse("")));
        let doc = parse(
            "<p><b>x</b><span style='mso-list:Ignore'>\u{a0} 2.<span>&nbsp;&nbsp;</span></span>text</p>",
        );
        let paragraph = find_in(&doc, "p");
        assert_eq!(find_marker(&paragraph).as_deref(), Some("2."));
        // Handles into a document are only valid while the document itself is alive.
        let plain_doc = parse("<p>a<i>b</i></p>");
        assert_eq!(find_marker(&find_in(&plain_doc, "p")), None);
        let nested_doc = parse("<p><span><i style='mso-list:ignore'>·</i></span></p>");
        assert_eq!(
            find_marker(&find_in(&nested_doc, "p")).as_deref(),
            Some("·")
        );
    }

    fn find_in(doc: &Handle, name: &str) -> Handle {
        let mut stack = vec![doc.clone()];
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
    fn marker_parsing_table() {
        let marker = |ordered, paren, start, lettered| Marker {
            ordered,
            paren,
            start,
            lettered,
        };
        let table = [
            ("1.", marker(true, false, 1, false)),
            ("12)", marker(true, true, 12, false)),
            ("(3)", marker(true, true, 3, false)),
            ("1.2.4.", marker(true, false, 4, false)),
            ("a)", marker(true, true, 1, true)),
            ("iv.", marker(true, false, 1, true)),
            ("(B)", marker(true, true, 1, true)),
            ("\u{b7}", marker(false, false, 1, false)),
            ("o", marker(false, false, 1, false)),
            ("\u{f0b7}", marker(false, false, 1, false)),
            (".", marker(false, false, 1, false)),
            ("\u{2022}.", marker(false, false, 1, false)),
            ("99999999999999999999.", marker(true, false, 1, false)),
            ("", marker(false, false, 1, false)),
        ];
        for (text, expected) in table {
            assert_eq!(parse_marker(text), expected, "{text:?}");
        }
    }

    #[test]
    fn heading_class_table() {
        let table = [
            ("MsoTitle", Some(1)),
            ("MsoSubtitle", Some(2)),
            ("x MsoHeading1", Some(1)),
            ("msoheading3 y", Some(3)),
            ("MsoHeading9", Some(6)),
            ("MsoHeading0", None),
            ("MsoHeadingX", None),
            ("MsoNormal", None),
            ("", None),
        ];
        for (class, expected) in table {
            assert_eq!(heading_level(class), expected, "{class}");
        }
    }

    #[test]
    fn highlight_table() {
        let table = [
            ("mso-highlight:yellow", true),
            ("mso-highlight:auto", false),
            ("mso-highlight:none", false),
            ("mso-highlight:windowtext", false),
            ("color:red", false),
        ];
        for (css, expected) in table {
            assert_eq!(is_highlight(&Style::parse(css)), expected, "{css}");
        }
    }
}
