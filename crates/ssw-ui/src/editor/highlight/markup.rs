//! Line scanners for markup-like languages: HTML/XML, CSS, Markdown and unified diffs.

use super::engine::escape_len;
use super::{Language, Mode, State, Token, TokenKind};

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

/// The elements whose body is raw text that another language scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Element {
    Script,
    Style,
}

impl Element {
    fn of(tag_name: &str) -> Option<Self> {
        if tag_name.eq_ignore_ascii_case("script") {
            Some(Self::Script)
        } else if tag_name.eq_ignore_ascii_case("style") {
            Some(Self::Style)
        } else {
            None
        }
    }

    /// The lowercase tag name, which the end tag repeats.
    fn name(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::Style => "style",
        }
    }
}

/// What the `type` attribute of a `<script>` or `<style>` says about its body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypeAttr {
    /// No `type`, or an empty one: the element's own default language.
    Absent,
    JavaScript,
    Json,
    Css,
    /// Any other type: data or a template for somebody else, shown as plain text.
    Other,
}

/// The JavaScript types of the WHATWG MIME Sniffing standard, and `module`.
const JAVASCRIPT_TYPES: &[&str] = &[
    "module",
    "application/ecmascript",
    "application/javascript",
    "application/x-ecmascript",
    "application/x-javascript",
    "text/ecmascript",
    "text/javascript",
    "text/javascript1.0",
    "text/javascript1.1",
    "text/javascript1.2",
    "text/javascript1.3",
    "text/javascript1.4",
    "text/javascript1.5",
    "text/jscript",
    "text/livescript",
    "text/x-ecmascript",
    "text/x-javascript",
];

impl TypeAttr {
    /// The meaning of an attribute value: types ignore case and surrounding whitespace.
    fn of(value: &str) -> Self {
        let value = value.trim_ascii();
        let is_one_of = |types: &[&str]| types.iter().any(|ty| value.eq_ignore_ascii_case(ty));
        if value.is_empty() {
            Self::Absent
        } else if is_one_of(JAVASCRIPT_TYPES) {
            Self::JavaScript
        } else if is_one_of(&["application/json", "importmap"])
            || ends_with_ignore_case(value, "+json")
        {
            Self::Json
        } else if value.eq_ignore_ascii_case("text/css") {
            Self::Css
        } else {
            Self::Other
        }
    }

    /// The language of the body of `element`, `None` for plain text.
    fn guest(self, element: Element) -> Option<Language> {
        match (element, self) {
            (Element::Script, Self::Absent | Self::JavaScript) => Some(Language::JavaScript),
            (Element::Script, Self::Json) => Some(Language::Json),
            (Element::Style, Self::Absent | Self::Css) => Some(Language::Css),
            _ => None,
        }
    }
}

/// Where the HTML scanner is in a `<script>` or `<style>` element, from its open tag to its end
/// tag. All of it is `Copy` and flat, so the highlighter can compare it per line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Embed {
    /// Not in such an element.
    #[default]
    None,
    /// In the open tag, which may span lines: the `type` seen so far decides the body.
    Open(Element, TypeAttr),
    /// In the body. The guest language (`None` for plain text) keeps its scanner state in
    /// `State::mode` and `State::depth`: the host has nothing of its own to remember until the
    /// end tag.
    Body(Element, Option<Language>),
}

fn ends_with_ignore_case(text: &str, suffix: &str) -> bool {
    let (text, suffix) = (text.as_bytes(), suffix.as_bytes());
    text.len() >= suffix.len() && text[text.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

/// The attribute value at the start of `text` (what follows the `=`), if it ends on this line.
fn attribute_value(text: &str) -> Option<&str> {
    let text = text.trim_ascii_start();
    match text.chars().next()? {
        quote @ ('"' | '\'') => text[1..].split_once(quote).map(|(value, _)| value),
        _ => {
            let end = text.find(|c: char| c.is_ascii_whitespace() || c == '>');
            Some(end.map_or(text, |end| &text[..end]))
        }
    }
}

/// The state after the `>` of a tag: the body of a `<script>` or `<style>` starts, any other
/// tag is followed by text.
fn after_tag(state: State) -> State {
    match state.embed {
        Embed::Open(element, ty) => State {
            embed: Embed::Body(element, ty.guest(element)),
            ..State::default()
        },
        _ => State::default(),
    }
}

/// Where the end tag of the raw-text element `name` starts in `text`: `</name` in any case,
/// followed by whitespace, `/`, `>` or the end of the line. Only this ends the element, whatever
/// the guest language is in the middle of (a string, a comment): that is what the HTML spec says
/// and what browsers do.
fn find_end_tag(text: &str, name: &str) -> Option<usize> {
    let name = name.as_bytes();
    let mut from = 0;
    while let Some(at) = text[from..].find("</") {
        let start = from + at;
        let tail = &text.as_bytes()[start + 2..];
        let named = tail
            .get(..name.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(name));
        let ended = tail
            .get(name.len())
            .is_none_or(|b| b" \t\n\r\x0C/>".contains(b));
        if named && ended {
            return Some(start);
        }
        from = start + 2;
    }
    None
}

/// Scans `line[from..]` in the body of `element`. The guest language gets the text up to the end
/// tag as a line of its own, so a `</script>` inside one of its strings or comments still ends
/// the element; at the end tag the guest state is dropped and the host resumes there. Returns
/// where the host resumes.
fn scan_body(
    line: &str,
    from: usize,
    element: Element,
    guest: Option<Language>,
    state: &mut State,
    out: &mut Vec<Token>,
) -> usize {
    let end_tag = find_end_tag(&line[from..], element.name());
    let end = end_tag.map_or(line.len(), |at| from + at);
    if let Some(language) = guest
        && from < end
    {
        let before = out.len();
        let inner = State {
            embed: Embed::None,
            ..*state
        };
        let next = super::scan_line(language, &line[from..end], inner, out);
        for token in &mut out[before..] {
            token.start += from;
            token.end += from;
        }
        *state = State {
            embed: Embed::Body(element, guest),
            ..next
        };
    }
    if end_tag.is_some() {
        *state = State::default();
    }
    end
}

/// HTML and XML: comments, tags, attributes, attribute values and character references. In HTML
/// (`html`) the body of a `<script>` or `<style>` is JavaScript, JSON or CSS as its `type` says;
/// XML has no raw-text elements (XAML has a `<Style>` that is no CSS).
pub(super) fn scan_markup(line: &str, state: State, html: bool, out: &mut Vec<Token>) -> State {
    let bytes = line.as_bytes();
    let len = line.len();
    let mut state = state;
    let mut i = 0;
    // The attribute name just before an `=` is `type`; a name and its `=` share a line in
    // practice, so this does not need to survive a line break.
    let mut after_type = false;
    while i < len {
        if let Embed::Body(element, guest) = state.embed {
            i = scan_body(line, i, element, guest, &mut state, out);
            continue;
        }
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
                    // Browsers ignore the slash of a non-void element: the body starts anyway.
                    state = after_tag(state);
                    i += 2;
                } else if ch == '>' {
                    state = after_tag(state);
                    i += 1;
                } else if ch == '=' {
                    if after_type
                        && let Embed::Open(element, TypeAttr::Absent) = state.embed
                        && let Some(value) = attribute_value(&rest[1..])
                    {
                        state.embed = Embed::Open(element, TypeAttr::of(value));
                    }
                    after_type = false;
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
                    after_type = rest[..end].eq_ignore_ascii_case("type");
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
                                ..State::default()
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
                        after_type = false;
                        if html
                            && name_from == 1
                            && let Some(element) = Element::of(&rest[1..=name_len])
                        {
                            state.embed = Embed::Open(element, TypeAttr::Absent);
                        }
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
            ..State::default()
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
