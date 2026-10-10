//! Lexical helpers shared by the block and inline passes. All positions are byte indices into
//! the slice that is passed in.

use crate::markdown::entities;
use crate::markdown::unicode_punct;

/// Unicode whitespace as used by the delimiter-run rules (Zs, tab, LF, FF, CR).
pub(super) fn is_unicode_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// ASCII punctuation or Unicode P*/S* character.
pub(super) fn is_punctuation(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_punctuation()
    } else {
        unicode_punct::is_non_ascii_punctuation(c)
    }
}

/// Space, tab, line feed, form feed, carriage return.
pub(super) fn is_html_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0c | b'\r')
}

/// Decodes the character reference at the start of `s` (`s[0] == '&'`). Returns the decoded
/// text and the number of bytes consumed.
pub(super) fn decode_entity(s: &[u8]) -> Option<(String, usize)> {
    if s.first() != Some(&b'&') {
        return None;
    }
    if s.get(1) == Some(&b'#') {
        let (hex, digits_start, max) = match s.get(2) {
            Some(b'x' | b'X') => (true, 3, 6),
            _ => (false, 2, 7),
        };
        let mut i = digits_start;
        let mut value: u32 = 0;
        while let Some(&c) = s.get(i) {
            let digit = if hex {
                char::from(c).to_digit(16)
            } else {
                char::from(c).to_digit(10)
            };
            let Some(d) = digit else { break };
            value = value
                .saturating_mul(if hex { 16 } else { 10 })
                .saturating_add(d);
            i += 1;
        }
        let count = i - digits_start;
        if count == 0 || count > max || s.get(i) != Some(&b';') {
            return None;
        }
        let ch = if value == 0 {
            '\u{fffd}'
        } else {
            char::from_u32(value).unwrap_or('\u{fffd}')
        };
        return Some((ch.to_string(), i + 1));
    }
    let mut i = 1;
    while let Some(&c) = s.get(i) {
        if c.is_ascii_alphanumeric() {
            i += 1;
        } else {
            break;
        }
    }
    if i == 1 || s.get(i) != Some(&b';') || i - 1 > 32 {
        return None;
    }
    let name = std::str::from_utf8(&s[1..i]).ok()?;
    let decoded = entities::lookup(name)?;
    Some((decoded.to_string(), i + 1))
}

/// Resolves backslash escapes and character references in `s`.
pub(super) fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' if b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                out.push(char::from(b[i + 1]));
                i += 2;
            }
            b'&' => {
                if let Some((text, used)) = decode_entity(&b[i..]) {
                    out.push_str(&text);
                    i += used;
                } else {
                    out.push('&');
                    i += 1;
                }
            }
            _ => {
                let ch_len = utf8_len(b[i]);
                out.push_str(&s[i..i + ch_len]);
                i += ch_len;
            }
        }
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Link label `[...]` starting at `b[pos] == '['`. Returns the position after `]`. The label
/// must contain a non-whitespace character and at most 999 characters, with no unescaped
/// brackets.
pub(super) fn link_label_end(b: &[u8], pos: usize) -> Option<usize> {
    if b.get(pos) != Some(&b'[') {
        return None;
    }
    let mut i = pos + 1;
    let mut non_space = false;
    while i < b.len() {
        match b[i] {
            b'\\' if b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => {
                non_space = true;
                i += 2;
                if i - pos > 1000 {
                    return None;
                }
                continue;
            }
            b'[' => return None,
            b']' => return non_space.then_some(i + 1),
            c if !is_html_space(c) => non_space = true,
            _ => {}
        }
        i += 1;
        if i - pos > 1000 {
            return None;
        }
    }
    None
}

/// Link destination. Returns `(raw range without angle brackets, end)`.
pub(super) fn link_destination(b: &[u8], pos: usize) -> Option<((usize, usize), usize)> {
    if b.get(pos) == Some(&b'<') {
        let mut i = pos + 1;
        while i < b.len() {
            match b[i] {
                b'\\' if b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => i += 2,
                b'>' => return Some(((pos + 1, i), i + 1)),
                b'<' | b'\n' => return None,
                _ => i += 1,
            }
        }
        return None;
    }
    let mut i = pos;
    let mut parens = 0usize;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\\' if b.get(i + 1).is_some_and(u8::is_ascii_punctuation) => i += 2,
            b'(' => {
                parens += 1;
                i += 1;
            }
            b')' => {
                if parens == 0 {
                    break;
                }
                parens -= 1;
                i += 1;
            }
            c if c <= 0x20 || c == 0x7f => break,
            _ => i += 1,
        }
    }
    if parens != 0 || (i == pos && b.get(i) != Some(&b')')) {
        return None;
    }
    Some(((pos, i), i))
}

/// Link title in `"…"`, `'…'` or `(…)`. Returns `(raw range without delimiters, end)`.
pub(super) fn link_title(b: &[u8], pos: usize) -> Option<((usize, usize), usize)> {
    let close = match b.get(pos)? {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let mut i = pos + 1;
    while i < b.len() {
        let c = b[i];
        if c == b'\\' && b.get(i + 1).is_some_and(u8::is_ascii_punctuation) {
            i += 2;
        } else if c == close {
            return Some(((pos + 1, i), i + 1));
        } else if c == b'(' && close == b')' {
            return None;
        } else {
            i += 1;
        }
    }
    None
}

/// Skips spaces, tabs and at most one line ending.
pub(super) fn skip_spaces_and_newline(b: &[u8], mut i: usize) -> usize {
    let mut seen_newline = false;
    while let Some(&c) = b.get(i) {
        match c {
            b' ' | b'\t' => i += 1,
            b'\n' if !seen_newline => {
                seen_newline = true;
                i += 1;
            }
            _ => break,
        }
    }
    i
}

/// Position of the first `needle` at or after `from` (callers keep `from <= b.len()`).
fn find_seq(b: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    b[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// End of the HTML tag, comment, processing instruction, declaration or CDATA section starting
/// at `b[pos] == '<'`.
pub(super) fn html_inline_end(b: &[u8], pos: usize) -> Option<usize> {
    if b.get(pos) != Some(&b'<') {
        return None;
    }
    let mut i = pos + 1;
    match *b.get(i)? {
        b'/' => closing_tag_end(b, pos),
        b'!' => {
            i += 1;
            if b[i..].starts_with(b"--") {
                let body = i + 2;
                if b[body..].starts_with(b">") {
                    return Some(body + 1);
                }
                if b[body..].starts_with(b"->") {
                    return Some(body + 2);
                }
                return find_seq(b, body, b"-->").map(|p| p + 3);
            }
            if b[i..].starts_with(b"[CDATA[") {
                return find_seq(b, i + 7, b"]]>").map(|p| p + 3);
            }
            if b.get(i).is_some_and(u8::is_ascii_alphabetic) {
                return b[i..].iter().position(|&c| c == b'>').map(|p| i + p + 1);
            }
            None
        }
        b'?' => find_seq(b, i + 1, b"?>").map(|p| p + 2),
        c if c.is_ascii_alphabetic() => open_tag_end(b, pos),
        _ => None,
    }
}

/// End of `</name\s*>` at `pos`.
pub(super) fn closing_tag_end(b: &[u8], pos: usize) -> Option<usize> {
    if b.get(pos) != Some(&b'<') || b.get(pos + 1) != Some(&b'/') {
        return None;
    }
    let mut i = pos + 2;
    if !b.get(i)?.is_ascii_alphabetic() {
        return None;
    }
    while b
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-')
    {
        i += 1;
    }
    while b.get(i).is_some_and(|&c| is_html_space(c)) {
        i += 1;
    }
    (b.get(i) == Some(&b'>')).then_some(i + 1)
}

/// End of an opening tag `<name attr…>` / `<name … />` at `pos`.
pub(super) fn open_tag_end(b: &[u8], pos: usize) -> Option<usize> {
    if b.get(pos) != Some(&b'<') || !b.get(pos + 1)?.is_ascii_alphabetic() {
        return None;
    }
    let mut i = pos + 2;
    while b
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-')
    {
        i += 1;
    }
    loop {
        let save = i;
        while b.get(i).is_some_and(|&c| is_html_space(c)) {
            i += 1;
        }
        if i == save {
            break;
        }
        match b.get(i) {
            Some(c) if c.is_ascii_alphabetic() || *c == b'_' || *c == b':' => {
                i += 1;
                while b.get(i).is_some_and(|c| {
                    c.is_ascii_alphanumeric() || matches!(*c, b'_' | b'.' | b':' | b'-')
                }) {
                    i += 1;
                }
                let mut j = i;
                while b.get(j).is_some_and(|&c| is_html_space(c)) {
                    j += 1;
                }
                if b.get(j) == Some(&b'=') {
                    j += 1;
                    while b.get(j).is_some_and(|&c| is_html_space(c)) {
                        j += 1;
                    }
                    match b.get(j)? {
                        &q if q == b'"' || q == b'\'' => {
                            let close = b[j + 1..].iter().position(|&c| c == q)?;
                            i = j + 1 + close + 1;
                        }
                        _ => {
                            let start = j;
                            while b.get(j).is_some_and(|&c| {
                                !is_html_space(c)
                                    && !matches!(c, b'"' | b'\'' | b'=' | b'<' | b'>' | b'`')
                            }) {
                                j += 1;
                            }
                            if j == start {
                                return None;
                            }
                            i = j;
                        }
                    }
                }
            }
            _ => {
                i = save;
                break;
            }
        }
    }
    while b.get(i).is_some_and(|&c| is_html_space(c)) {
        i += 1;
    }
    if b.get(i) == Some(&b'/') {
        i += 1;
    }
    (b.get(i) == Some(&b'>')).then_some(i + 1)
}

/// Value of attribute `name` (case-insensitive) in the opening tag `tag` (`<img …>`),
/// as a byte range of `tag`.
pub(super) fn tag_attribute(tag: &[u8], name: &str) -> Option<(usize, usize)> {
    let mut i = 1;
    while tag
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'-')
    {
        i += 1;
    }
    loop {
        while tag.get(i).is_some_and(|&c| is_html_space(c)) {
            i += 1;
        }
        let name_start = i;
        while tag
            .get(i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(*c, b'_' | b'.' | b':' | b'-'))
        {
            i += 1;
        }
        if i == name_start {
            return None;
        }
        let attr = &tag[name_start..i];
        let mut j = i;
        while tag.get(j).is_some_and(|&c| is_html_space(c)) {
            j += 1;
        }
        let mut value = (i, i);
        if tag.get(j) == Some(&b'=') {
            j += 1;
            while tag.get(j).is_some_and(|&c| is_html_space(c)) {
                j += 1;
            }
            match tag.get(j)? {
                &q if q == b'"' || q == b'\'' => {
                    let close = tag[j + 1..].iter().position(|&c| c == q)?;
                    value = (j + 1, j + 1 + close);
                    i = j + 1 + close + 1;
                }
                _ => {
                    let start = j;
                    while tag.get(j).is_some_and(|&c| {
                        !is_html_space(c) && !matches!(c, b'"' | b'\'' | b'=' | b'<' | b'>' | b'`')
                    }) {
                        j += 1;
                    }
                    value = (start, j);
                    i = j;
                }
            }
        }
        if attr.eq_ignore_ascii_case(name.as_bytes()) {
            return Some(value);
        }
    }
}

/// Parses a pixel length such as `300` or `300px`.
pub(super) fn parse_pixels(s: &str) -> Option<u32> {
    let s = s.trim();
    let digits = s.strip_suffix("px").unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}
