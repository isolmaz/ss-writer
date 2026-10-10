//! Line scanners for markup-like languages: HTML/XML, CSS, Markdown and unified diffs.

use super::engine::escape_len;
use super::{Mode, State, Token, TokenKind};

fn push(out: &mut Vec<Token>, start: usize, end: usize, kind: TokenKind) {
    if start < end {
        out.push(Token { start, end, kind });
    }
}

fn char_len(rest: &str) -> usize {
    rest.chars().next().map_or(1, char::len_utf8)
}

fn is_name_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | ':' | '.' | '-')
}

/// Pushes an escape token for every character reference (`&amp;`, `&#35;`) in `line[from..to]`.
fn entities(line: &str, from: usize, to: usize, out: &mut Vec<Token>) {
    let mut j = from;
    while let Some(at) = line[j..to].find('&') {
        let start = j + at;
        let name = &line[start + 1..to];
        let len = name
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .unwrap_or(name.len());
        if len > 0 && name[len..].starts_with(';') {
            push(out, start, start + len + 2, TokenKind::Escape);
            j = start + len + 2;
        } else {
            j = start + 1;
        }
    }
}

/// HTML and XML: comments, tags, attributes, attribute values and character references.
pub(super) fn scan_markup(line: &str, state: State, out: &mut Vec<Token>) -> State {
    let bytes = line.as_bytes();
    let len = line.len();
    let mut state = state;
    let mut i = 0;
    while i < len {
        match state.mode {
            Mode::Comment => match line[i..].find("-->") {
                Some(at) => {
                    push(out, i, i + at + 3, TokenKind::Comment);
                    i += at + 3;
                    state = State::default();
                }
                None => {
                    push(out, i, len, TokenKind::Comment);
                    return state;
                }
            },
            Mode::TagStr(quote) => match line[i..].find(char::from(quote)) {
                Some(at) => {
                    push(out, i, i + at + 1, TokenKind::String);
                    i += at + 1;
                    state.mode = Mode::Tag;
                }
                None => {
                    push(out, i, len, TokenKind::String);
                    return state;
                }
            },
            Mode::Tag => {
                let rest = &line[i..];
                let ch = rest.chars().next().unwrap_or(' ');
                if rest.starts_with("/>") {
                    state = State::default();
                    i += 2;
                } else if ch == '>' {
                    state = State::default();
                    i += 1;
                } else if ch == '"' || ch == '\'' {
                    match rest[1..].find(ch) {
                        Some(at) => {
                            push(out, i, i + at + 2, TokenKind::String);
                            i += at + 2;
                        }
                        None => {
                            push(out, i, len, TokenKind::String);
                            state.mode = Mode::TagStr(bytes[i]);
                            return state;
                        }
                    }
                } else if is_name_char(ch) {
                    let end = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
                    push(out, i, i + end, TokenKind::Attribute);
                    i += end;
                } else {
                    i += ch.len_utf8();
                }
            }
            _ => {
                let Some(at) = line[i..].find('<') else {
                    entities(line, i, len, out);
                    return state;
                };
                let open = i + at;
                entities(line, i, open, out);
                let rest = &line[open..];
                if let Some(body) = rest.strip_prefix("<!--") {
                    match body.find("-->") {
                        Some(end) => {
                            push(out, open, open + end + 7, TokenKind::Comment);
                            i = open + end + 7;
                        }
                        None => {
                            push(out, open, len, TokenKind::Comment);
                            return State {
                                mode: Mode::Comment,
                                depth: 0,
                            };
                        }
                    }
                } else if rest.starts_with("<!") || rest.starts_with("<?") {
                    let end = rest.find('>').map_or(len, |close| open + close + 1);
                    push(out, open, end, TokenKind::Keyword);
                    i = end;
                } else {
                    let name_from = if rest.starts_with("</") { 2 } else { 1 };
                    let name_len = rest[name_from..]
                        .find(|c: char| !is_name_char(c))
                        .unwrap_or(rest.len() - name_from);
                    if name_len == 0 {
                        i = open + 1;
                    } else {
                        let end = open + name_from + name_len;
                        push(out, open, end, TokenKind::Tag);
                        state.mode = Mode::Tag;
                        i = end;
                    }
                }
            }
        }
    }
    state
}

/// CSS: comments, at-rules, selectors, properties, values, numbers with units and colors.
/// `state.depth` is the brace depth.
pub(super) fn scan_css(line: &str, state: State, out: &mut Vec<Token>) -> State {
    let bytes = line.as_bytes();
    let len = line.len();
    let mut state = state;
    let mut i = 0;
    let ident = |c: char| c.is_alphanumeric() || c == '-' || c == '_';
    while i < len {
        if state.mode == Mode::Comment {
            match line[i..].find("*/") {
                Some(at) => {
                    push(out, i, i + at + 2, TokenKind::Comment);
                    i += at + 2;
                    state.mode = Mode::Normal;
                }
                None => {
                    push(out, i, len, TokenKind::Comment);
                    return state;
                }
            }
            continue;
        }
        let rest = &line[i..];
        let ch = rest.chars().next().unwrap_or(' ');
        let next = rest[ch.len_utf8()..].chars().next();
        if ch.is_whitespace() {
            i += ch.len_utf8();
        } else if let Some(body) = rest.strip_prefix("/*") {
            state.mode = Mode::Comment;
            match body.find("*/") {
                Some(at) => {
                    push(out, i, i + at + 4, TokenKind::Comment);
                    i += at + 4;
                    state.mode = Mode::Normal;
                }
                None => {
                    push(out, i, len, TokenKind::Comment);
                    return state;
                }
            }
        } else if ch == '"' || ch == '\'' {
            let mut j = i + 1;
            while j < len && bytes[j] != bytes[i] {
                j += if bytes[j] == b'\\' {
                    escape_len(&line[j..])
                } else {
                    char_len(&line[j..])
                };
            }
            let end = (j + 1).min(len);
            push(out, i, end, TokenKind::String);
            i = end;
        } else if ch == '{' {
            state.depth = state.depth.saturating_add(1);
            i += 1;
        } else if ch == '}' {
            state.depth = state.depth.saturating_sub(1);
            i += 1;
        } else if ch == '@' || (ch == '!' && rest[1..].trim_start().starts_with("important")) {
            let skip = if ch == '!' {
                1 + rest[1..].len() - rest[1..].trim_start().len()
            } else {
                1
            };
            let end = rest[skip..]
                .find(|c: char| !ident(c))
                .unwrap_or(rest.len() - skip);
            push(out, i, i + skip + end, TokenKind::Keyword);
            i += skip + end;
        } else if ch.is_ascii_digit()
            || (matches!(ch, '.' | '-' | '+') && next.is_some_and(|c| c.is_ascii_digit()))
        {
            let mut j = i + 1;
            while j < len && (bytes[j].is_ascii_digit() || bytes[j] == b'.') {
                j += 1;
            }
            while j < len && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'%') {
                j += 1;
            }
            push(out, i, j, TokenKind::Number);
            i = j;
        } else if ch == '#' && state.depth > 0 {
            let end = rest[1..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .unwrap_or(rest.len() - 1);
            push(out, i, i + 1 + end, TokenKind::Number);
            i += 1 + end;
        } else if (ch == '#' || ch == '.') && state.depth == 0 && next.is_some_and(ident) {
            let end = rest[1..]
                .find(|c: char| !ident(c))
                .unwrap_or(rest.len() - 1);
            push(out, i, i + 1 + end, TokenKind::Type);
            i += 1 + end;
        } else if ident(ch) {
            let end = rest.find(|c: char| !ident(c)).unwrap_or(rest.len());
            let after = rest[end..].trim_start();
            let kind = if after.starts_with('(') {
                TokenKind::Function
            } else if state.depth > 0 && after.starts_with(':') && !after.starts_with("::") {
                TokenKind::Property
            } else if state.depth == 0 && line[..i].ends_with(':') {
                TokenKind::Attribute
            } else if state.depth == 0 {
                TokenKind::Tag
            } else {
                TokenKind::Plain
            };
            push(out, i, i + end, kind);
            i += end;
        } else {
            i += ch.len_utf8();
        }
    }
    state
}

/// Markdown: headings, quotes, list markers, rules, fences, code spans, links and strong text.
pub(super) fn scan_markdown(line: &str, state: State, out: &mut Vec<Token>) -> State {
    let len = line.len();
    let trimmed = line.trim_start();
    let indent = len - trimmed.len();
    let fence = ["```", "~~~"]
        .iter()
        .find(|fence| trimmed.starts_with(**fence));
    if state.mode == Mode::Fence {
        if let Some(fence) = fence {
            push(out, indent, indent + fence.len(), TokenKind::Comment);
            return State::default();
        }
        push(out, 0, len, TokenKind::String);
        return state;
    }
    if let Some(fence) = fence {
        let marks = trimmed
            .chars()
            .take_while(|&c| c == fence.as_bytes()[0] as char)
            .count();
        push(out, indent, indent + marks, TokenKind::Comment);
        push(out, indent + marks, len, TokenKind::Type);
        return State {
            mode: Mode::Fence,
            depth: 0,
        };
    }
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes)
        && trimmed[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace)
    {
        push(out, indent, len, TokenKind::Heading);
        return state;
    }
    let mut from = indent;
    if trimmed.starts_with('>') {
        push(out, indent, indent + 1, TokenKind::Comment);
        from = indent + 1;
    } else if let Some(marker) = list_marker(trimmed) {
        push(out, indent, indent + marker, TokenKind::Keyword);
        from = indent + marker;
    } else if trimmed.len() >= 3
        && trimmed.chars().all(|c| c == trimmed.as_bytes()[0] as char)
        && matches!(trimmed.as_bytes()[0], b'-' | b'*' | b'_')
    {
        push(out, indent, len, TokenKind::Keyword);
        return state;
    }
    inline(line, from, out);
    state
}

/// Length of the list marker (`-`, `*`, `+`, `1.`, `1)`) at the start of `text` including the
/// space after it.
fn list_marker(text: &str) -> Option<usize> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    let marker = match text.as_bytes().get(digits)? {
        b'-' | b'*' | b'+' if digits == 0 => 1,
        b'.' | b')' if digits > 0 => digits + 1,
        _ => return None,
    };
    text[marker..].starts_with(' ').then_some(marker + 1)
}

fn inline(line: &str, from: usize, out: &mut Vec<Token>) {
    let bytes = line.as_bytes();
    let len = line.len();
    let mut i = from;
    while i < len {
        match bytes[i] {
            b'`' => {
                let ticks = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                let closer = "`".repeat(ticks);
                match line[i + ticks..].find(&closer) {
                    Some(at) => {
                        let end = i + ticks + at + ticks;
                        push(out, i, end, TokenKind::String);
                        i = end;
                    }
                    None => i += ticks,
                }
            }
            b'[' => match line[i..].find("](") {
                Some(text_end) => {
                    let url_start = i + text_end + 1;
                    match line[url_start..].find(')') {
                        Some(url_end) => {
                            push(out, i, i + text_end + 1, TokenKind::Function);
                            push(out, url_start, url_start + url_end + 1, TokenKind::Constant);
                            i = url_start + url_end + 1;
                        }
                        None => i += 1,
                    }
                }
                None => i += 1,
            },
            b'*' | b'_' if line[i..].starts_with("**") || line[i..].starts_with("__") => {
                let marker = &line[i..i + 2];
                match line[i + 2..].find(marker) {
                    Some(at) if at > 0 => {
                        let end = i + 2 + at + 2;
                        push(out, i, end, TokenKind::Variable);
                        i = end;
                    }
                    _ => i += 2,
                }
            }
            _ => i += char_len(&line[i..]),
        }
    }
}

/// Unified and context diffs: file headers, hunk headers, added and removed lines.
pub(super) fn scan_diff(line: &str, out: &mut Vec<Token>) {
    let len = line.len();
    let header = |prefix: &str| line.starts_with(prefix);
    let kind = if header("+++ ") || header("--- ") {
        TokenKind::Keyword
    } else if header("@@") {
        TokenKind::Type
    } else if [
        "diff ",
        "index ",
        "new file",
        "deleted file",
        "similarity",
        "rename ",
        "Binary files",
    ]
    .iter()
    .any(|prefix| header(prefix))
    {
        TokenKind::Comment
    } else if header("+") {
        TokenKind::Added
    } else if header("-") {
        TokenKind::Removed
    } else {
        return;
    };
    push(out, 0, len, kind);
}
