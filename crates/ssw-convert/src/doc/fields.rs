//! Fields (`0x13 instruction 0x14 result 0x15`): instruction parsing and classification, and the
//! small text helpers the field and symbol handling needs.

/// What a field instruction turns into.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FieldKind {
    /// `HYPERLINK "url"`: the result becomes the link text.
    Hyperlink(String),
    /// `HYPERLINK \l "bookmark"` (or without a target): the result is kept as plain text.
    InternalLink,
    /// TOC, PAGEREF, PAGE, XE, ...: the result is dropped.
    Suppressed,
    /// Any other field: the result text is kept.
    Plain,
}

/// Splits a field instruction into whitespace-separated tokens; double quotes group.
pub(super) fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut have = false;
    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                have = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}

pub(super) fn classify(instr: &str) -> FieldKind {
    let toks = tokenize(instr);
    let keyword = toks
        .first()
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    match keyword.as_str() {
        "HYPERLINK" => {
            let mut url: Option<&str> = None;
            let mut i = 1;
            while i < toks.len() {
                let t = toks[i].as_str();
                if let Some(sw) = t.strip_prefix('\\') {
                    // switches that take an argument: \l \m \n \o \t
                    if sw.len() == 1 && "lmnotLMNOT".contains(sw) {
                        i += 1;
                    }
                } else if url.is_none() {
                    url = Some(t);
                }
                i += 1;
            }
            match url {
                Some(u) if !u.is_empty() => FieldKind::Hyperlink(clean_url(u)),
                _ => FieldKind::InternalLink,
            }
        }
        "TOC" | "TOA" | "XE" | "TC" | "TA" | "INDEX" | "PAGEREF" | "PAGE" | "NUMPAGES"
        | "SECTIONPAGES" => FieldKind::Suppressed,
        _ => FieldKind::Plain,
    }
}

/// Makes a URL safe inside a Markdown inline link destination.
pub(super) fn clean_url(u: &str) -> String {
    let mut o = String::with_capacity(u.len());
    for c in u.trim().chars() {
        match c {
            ' ' => o.push_str("%20"),
            '(' => o.push_str("%28"),
            ')' => o.push_str("%29"),
            '<' => o.push_str("%3C"),
            '>' => o.push_str("%3E"),
            c if (c as u32) < 0x20 => {}
            c => o.push(c),
        }
    }
    o
}

/// Maps the private-use code points of the Symbol/Wingdings bullets to Unicode.
pub(super) fn symbol_to_unicode(c: u16) -> u16 {
    match c {
        0xF0B7 | 0x00B7 => 0x2022,
        0xF0A7 => 0x25AA,
        0xF0D8 => 0x27A2,
        0xF076 => 0x2756,
        0xF0FC => 0x2713,
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_with_quotes() {
        assert_eq!(
            tokenize(r#" HYPERLINK  "http://a b/" \t "x y" "" "#),
            vec!["HYPERLINK", "http://a b/", "\\t", "x y", ""]
        );
        assert!(tokenize("   ").is_empty());
        assert_eq!(tokenize("a\tb"), vec!["a", "b"]);
    }

    #[test]
    fn classifies_instructions() {
        let cases: [(&str, FieldKind); 15] = [
            // a switch without an argument, and a second target that is ignored
            (
                "HYPERLINK \\h \"http://x.org\" \"http://y.org\"",
                FieldKind::Hyperlink("http://x.org".into()),
            ),
            (
                " HYPERLINK \"http://example.com/a b\" ",
                FieldKind::Hyperlink("http://example.com/a%20b".into()),
            ),
            (
                "hyperlink http://x.org/(1)",
                FieldKind::Hyperlink("http://x.org/%281%29".into()),
            ),
            ("HYPERLINK \\l \"bm\"", FieldKind::InternalLink),
            (
                "HYPERLINK \"http://x.org\" \\l \"bm\" \\t \"_blank\"",
                FieldKind::Hyperlink("http://x.org".into()),
            ),
            ("HYPERLINK \"\"", FieldKind::InternalLink),
            ("HYPERLINK", FieldKind::InternalLink),
            (
                "HYPERLINK \\o \"tip\" \"http://y\"",
                FieldKind::Hyperlink("http://y".into()),
            ),
            ("TOC \\o \"1-3\"", FieldKind::Suppressed),
            (" PAGEREF _Toc1 \\h", FieldKind::Suppressed),
            ("PAGE", FieldKind::Suppressed),
            ("xe \"entry\"", FieldKind::Suppressed),
            ("DATE \\@ \"d MMMM\"", FieldKind::Plain),
            ("", FieldKind::Plain),
            ("HYPERLINK \\n http://z", FieldKind::InternalLink),
        ];
        for (instr, want) in cases {
            assert_eq!(classify(instr), want, "{instr:?}");
        }
    }

    #[test]
    fn urls_and_symbols() {
        assert_eq!(
            clean_url("  http://a/<b>\u{1}(c) d "),
            "http://a/%3Cb%3E%28c%29%20d"
        );
        let cases = [
            (0xF0B7u16, 0x2022u16),
            (0x00B7, 0x2022),
            (0xF0A7, 0x25AA),
            (0xF0D8, 0x27A2),
            (0xF076, 0x2756),
            (0xF0FC, 0x2713),
            (0x41, 0x41),
        ];
        for (from, to) in cases {
            assert_eq!(symbol_to_unicode(from), to);
        }
    }
}
