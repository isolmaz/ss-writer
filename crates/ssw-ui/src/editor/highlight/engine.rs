//! The generic line scanner for C-like and scripting languages, driven by a [`Spec`].

use super::{Mode, State, Token, TokenKind};

/// How `$name` and `${expr}` inside a string are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Interp {
    /// Not interpolated.
    No,
    /// `${expr}` (JavaScript template literals): the delimiters are escapes.
    Brace,
    /// `$name` / `${name}` (shell, PowerShell): variables.
    Dollar,
}

/// A string literal form.
#[derive(Debug)]
pub(super) struct StrRule {
    pub open: &'static str,
    pub close: &'static str,
    /// Backslash escapes are colored separately.
    pub escape: bool,
    /// The string continues on the next line when unterminated.
    pub multiline: bool,
    /// A doubled closing delimiter stands for itself (SQL `''`, C# `""`).
    pub doubled: bool,
    pub interp: Interp,
}

/// How attributes / decorators are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Attr {
    No,
    /// `#[derive(..)]`, `#![allow(..)]`.
    Rust,
    /// `@name` (Python decorators, Java annotations, TypeScript decorators).
    At,
}

/// Keys at the start of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KeyStyle {
    No,
    /// `key: value`, `- key: value`, `---`.
    Yaml,
    /// `key = value`, `[table]`.
    Toml,
}

/// Everything the scanner needs to know about a language.
#[derive(Debug)]
pub(super) struct Spec {
    pub keywords: &'static [&'static str],
    pub types: &'static [&'static str],
    pub constants: &'static [&'static str],
    /// Well-known functions and commands.
    pub builtins: &'static [&'static str],
    pub line_comments: &'static [&'static str],
    pub block_comment: Option<(&'static str, &'static str)>,
    pub nested_comments: bool,
    pub strings: &'static [StrRule],
    /// Letters that may precede a quote (`r"…"`, `b'…'`); a prefix containing `r` is raw.
    pub prefixes: &'static [&'static str],
    /// Characters besides letters, digits and `_` allowed in identifiers (`$` also starts one).
    pub ident_extra: &'static str,
    pub case_insensitive: bool,
    /// `Name` (capitalized, with a lowercase letter) is a type.
    pub capitalized_types: bool,
    /// `NAME` (all caps) is a constant.
    pub caps_constants: bool,
    /// `name(` is a function.
    pub call_is_function: bool,
    /// `name!` is a macro call (Rust).
    pub bang_macro: bool,
    /// `$name`, `${name}` and `$1` are variables.
    pub var_prefix: bool,
    /// `:` continues a variable name (`$env:PATH`).
    pub var_colon: bool,
    /// `-x` / `--long` after whitespace are options.
    pub flags: bool,
    /// `#include`-style directives at the start of a line.
    pub preproc: bool,
    pub attributes: Attr,
    /// A string followed by `:` is a property key (JSON).
    pub json_keys: bool,
    /// Rust character literals, lifetimes and raw strings.
    pub rust_lexical: bool,
    /// A line comment must start a word (`#` in shell).
    pub comment_boundary: bool,
    /// `Verb-Noun` words are cmdlets (PowerShell).
    pub verb_noun: bool,
    pub key: KeyStyle,
    /// Numbers continue through date and time characters (TOML).
    pub date_numbers: bool,
}

/// A spec with nothing set; languages override what they need with `..BASE`.
pub(super) const BASE: Spec = Spec {
    keywords: &[],
    types: &[],
    constants: &[],
    builtins: &[],
    line_comments: &[],
    block_comment: None,
    nested_comments: false,
    strings: &[],
    prefixes: &[],
    ident_extra: "",
    case_insensitive: false,
    capitalized_types: false,
    caps_constants: false,
    call_is_function: false,
    bang_macro: false,
    var_prefix: false,
    var_colon: false,
    flags: false,
    preproc: false,
    attributes: Attr::No,
    json_keys: false,
    rust_lexical: false,
    comment_boundary: false,
    verb_noun: false,
    key: KeyStyle::No,
    date_numbers: false,
};

/// Scans one line (without its terminator) starting in `state`; pushes the non-plain tokens
/// with offsets relative to the line and returns the state at the end of the line.
pub(super) fn scan_line(spec: &Spec, line: &str, state: State, out: &mut Vec<Token>) -> State {
    let mut scanner = Scanner {
        spec,
        line,
        out,
        state,
    };
    scanner.run();
    scanner.state
}

/// Length in bytes of the escape sequence at the start of `rest` (which starts with `\`).
pub(super) fn escape_len(rest: &str) -> usize {
    let mut chars = rest.chars();
    chars.next();
    let Some(c) = chars.next() else {
        return 1;
    };
    let mut len = 1 + c.len_utf8();
    let tail = &rest[len..];
    let hex = |max: usize| {
        tail.bytes()
            .take(max)
            .take_while(u8::is_ascii_hexdigit)
            .count()
    };
    match c {
        'x' => len += hex(2),
        'u' => match tail.strip_prefix('{').and_then(|inner| inner.find('}')) {
            Some(close) => len += close + 2,
            None => len += hex(4),
        },
        'U' => len += hex(8),
        '0'..='7' => {
            len += tail
                .bytes()
                .take(2)
                .take_while(|b| (b'0'..=b'7').contains(b))
                .count();
        }
        _ => {}
    }
    len
}

struct Scanner<'a> {
    spec: &'a Spec,
    line: &'a str,
    out: &'a mut Vec<Token>,
    state: State,
}

impl<'a> Scanner<'a> {
    fn push(&mut self, start: usize, end: usize, kind: TokenKind) {
        if start < end {
            self.out.push(Token { start, end, kind });
        }
    }

    fn run(&mut self) {
        // Only languages with block comments ever end a line inside one.
        let mut i = match (self.state.mode, self.spec.block_comment) {
            (Mode::Comment, Some(delimiters)) => self.comment_body(delimiters, 0, 0),
            (Mode::Str(rule), _) => {
                let raw = self.state.depth == 1;
                self.string_body(0, usize::from(rule), 0, raw)
            }
            (Mode::RawStr, _) => self.raw_body(0, 0, usize::from(self.state.depth)),
            _ => self.line_start(),
        };
        while let Some(ch) = self.line.get(i..).and_then(|rest| rest.chars().next()) {
            i = self.token(i, ch);
        }
    }

    fn is_start(&self, ch: char) -> bool {
        ch.is_alphabetic() || ch == '_' || (ch != '-' && self.spec.ident_extra.contains(ch))
    }

    fn is_cont(&self, ch: char) -> bool {
        ch.is_alphanumeric() || ch == '_' || self.spec.ident_extra.contains(ch)
    }

    /// End of the identifier that starts at `i`.
    fn ident_end(&self, i: usize) -> usize {
        let rest = &self.line[i..];
        let len = rest
            .char_indices()
            .find(|&(_, ch)| !self.is_cont(ch))
            .map_or(rest.len(), |(at, _)| at);
        i + len
    }

    /// Scans the rest of a block comment delimited by `(open, close)`.
    fn comment_body(
        &mut self,
        (open, close): (&str, &str),
        token_start: usize,
        from: usize,
    ) -> usize {
        let line = self.line;
        let mut j = from;
        while j < line.len() {
            let rest = &line[j..];
            if self.spec.nested_comments && rest.starts_with(open) {
                self.state.depth = self.state.depth.saturating_add(1);
                j += open.len();
            } else if rest.starts_with(close) {
                self.state.depth = self.state.depth.saturating_sub(1);
                j += close.len();
                if self.state.depth == 0 {
                    self.push(token_start, j, TokenKind::Comment);
                    self.state.mode = Mode::Normal;
                    return j;
                }
            } else {
                j += rest.chars().next().map_or(1, char::len_utf8);
            }
        }
        self.push(token_start, line.len(), TokenKind::Comment);
        line.len()
    }

    /// Scans a string of rule `rule_index` whose token starts at `token_start` and whose body
    /// starts at `from`. Returns the index after the string (the end of the line when it is
    /// unterminated; the state then says whether it continues).
    fn string_body(
        &mut self,
        token_start: usize,
        rule_index: usize,
        from: usize,
        raw: bool,
    ) -> usize {
        let line = self.line;
        let spec: &'a Spec = self.spec;
        let rule = &spec.strings[rule_index];
        let bytes = line.as_bytes();
        let mut j = from;
        let mut run = token_start;
        self.state = State::default();
        while j < line.len() {
            let rest = &line[j..];
            if rule.escape && !raw && bytes[j] == b'\\' {
                self.push(run, j, TokenKind::String);
                let len = escape_len(rest);
                self.push(j, j + len, TokenKind::Escape);
                j += len;
                run = j;
            } else if rest.starts_with(rule.close) {
                let width = rule.close.len();
                if rule.doubled && rest[width..].starts_with(rule.close) {
                    self.push(run, j, TokenKind::String);
                    self.push(j, j + 2 * width, TokenKind::Escape);
                    j += 2 * width;
                    run = j;
                } else {
                    j += width;
                    self.push(run, j, TokenKind::String);
                    return j;
                }
            } else if rule.interp == Interp::Brace && rest.starts_with("${") {
                match rest.find('}') {
                    Some(close) => {
                        self.push(run, j, TokenKind::String);
                        self.push(j, j + 2, TokenKind::Escape);
                        self.push(j + close, j + close + 1, TokenKind::Escape);
                        j += close + 1;
                        run = j;
                    }
                    None => j += 2,
                }
            } else if rule.interp == Interp::Dollar && bytes[j] == b'$' {
                let len = self.var_len(rest);
                if len > 1 {
                    self.push(run, j, TokenKind::String);
                    self.push(j, j + len, TokenKind::Variable);
                    j += len;
                    run = j;
                } else {
                    j += 1;
                }
            } else {
                j += rest.chars().next().map_or(1, char::len_utf8);
            }
        }
        self.push(run, line.len(), TokenKind::String);
        if rule.multiline {
            self.state = State {
                mode: Mode::Str(u8::try_from(rule_index).unwrap_or(u8::MAX)),
                depth: u8::from(raw),
            };
        }
        line.len()
    }

    /// Scans the rest of a Rust raw string with `hashes` closing hashes.
    fn raw_body(&mut self, token_start: usize, from: usize, hashes: usize) -> usize {
        let line = self.line;
        let mut close = String::from("\"");
        close.push_str(&"#".repeat(hashes));
        self.state = State::default();
        match line[from..].find(&close) {
            Some(at) => {
                let end = from + at + close.len();
                self.push(token_start, end, TokenKind::String);
                end
            }
            None => {
                self.push(token_start, line.len(), TokenKind::String);
                self.state = State {
                    mode: Mode::RawStr,
                    depth: u8::try_from(hashes).unwrap_or(u8::MAX),
                };
                line.len()
            }
        }
    }

    /// Length of the variable reference at the start of `rest` (which starts with `$`); 1 when
    /// it is a lone dollar sign.
    fn var_len(&self, rest: &str) -> usize {
        let after = &rest[1..];
        if let Some(inner) = after.strip_prefix('{') {
            return inner.find('}').map_or(1, |close| close + 3);
        }
        let Some(first) = after.chars().next() else {
            return 1;
        };
        if first.is_ascii_digit() || "?@#!$*".contains(first) {
            return 2;
        }
        if !(first.is_alphabetic() || first == '_') {
            return 1;
        }
        let mut len = 1;
        let mut chars = after.char_indices().peekable();
        while let Some((at, ch)) = chars.next() {
            let colon = self.spec.var_colon
                && ch == ':'
                && chars
                    .peek()
                    .is_some_and(|&(_, next)| next.is_alphanumeric());
            if !(ch.is_alphanumeric() || ch == '_' || colon) {
                break;
            }
            len = 1 + at + ch.len_utf8();
        }
        len
    }

    /// Keys, headers and document markers at the start of a line; returns where scanning
    /// continues.
    fn line_start(&mut self) -> usize {
        let line = self.line;
        let indent = line.len() - line.trim_start().len();
        match self.spec.key {
            KeyStyle::No => 0,
            KeyStyle::Yaml => {
                let mut j = indent;
                while line[j..].starts_with("- ") {
                    j += 2;
                    j += line[j..].len() - line[j..].trim_start().len();
                }
                if indent == 0
                    && (line.starts_with("---") || line.starts_with("..."))
                    && line[3..].trim().is_empty()
                {
                    self.push(0, 3, TokenKind::Keyword);
                    return 3;
                }
                match yaml_key_end(&line[j..]) {
                    Some(len) => {
                        self.push(j, j + len, TokenKind::Property);
                        j + len
                    }
                    None => j,
                }
            }
            KeyStyle::Toml => {
                let rest = &line[indent..];
                if rest.starts_with('[') {
                    let end = rest.rfind(']').map_or(rest.len(), |at| at + 1);
                    self.push(indent, indent + end, TokenKind::Type);
                    return indent + end;
                }
                match toml_key_end(rest) {
                    Some(len) => {
                        self.push(indent, indent + len, TokenKind::Property);
                        indent + len
                    }
                    None => indent,
                }
            }
        }
    }

    /// Scans the token at `i`, which starts with `ch`; returns the index after it.
    fn token(&mut self, i: usize, ch: char) -> usize {
        let line = self.line;
        let rest = &line[i..];
        if ch.is_whitespace() {
            return i + ch.len_utf8();
        }
        let boundary = i == 0 || line[..i].ends_with(char::is_whitespace);
        if (boundary || !self.spec.comment_boundary)
            && self
                .spec
                .line_comments
                .iter()
                .any(|comment| rest.starts_with(comment))
        {
            self.push(i, line.len(), TokenKind::Comment);
            return line.len();
        }
        if let Some(delimiters) = self.spec.block_comment
            && rest.starts_with(delimiters.0)
        {
            self.state = State {
                mode: Mode::Comment,
                depth: 1,
            };
            return self.comment_body(delimiters, i, i + delimiters.0.len());
        }
        if let Some(index) = self
            .spec
            .strings
            .iter()
            .position(|rule| rest.starts_with(rule.open))
        {
            return self.string_token(i, index, i, false);
        }
        if ch.is_ascii_digit() {
            let end = self.number_end(i);
            self.push(i, end, TokenKind::Number);
            return end;
        }
        if self.is_start(ch) {
            return self.word(i);
        }
        if ch == '$' && self.spec.var_prefix {
            let len = self.var_len(rest);
            if len > 1 {
                let name = rest[1..len].trim_matches(['{', '}']);
                let kind = if self.spec.case_insensitive
                    && ["true", "false", "null"]
                        .iter()
                        .any(|constant| name.eq_ignore_ascii_case(constant))
                {
                    TokenKind::Constant
                } else {
                    TokenKind::Variable
                };
                self.push(i, i + len, kind);
                return i + len;
            }
        }
        if ch == '@'
            && self.spec.attributes == Attr::At
            && rest[1..]
                .chars()
                .next()
                .is_some_and(|next| self.is_start(next))
        {
            let end = rest[1..]
                .find(|c: char| !(self.is_cont(c) || c == '.'))
                .map_or(rest.len(), |at| at + 1);
            self.push(i, i + end, TokenKind::Attribute);
            return i + end;
        }
        if self.spec.attributes == Attr::Rust && (rest.starts_with("#[") || rest.starts_with("#!["))
        {
            return self.rust_attribute(i);
        }
        if ch == '#' && self.spec.preproc && line[..i].trim().is_empty() {
            return self.directive(i);
        }
        if ch == '\'' && self.spec.rust_lexical {
            return self.rust_quote(i);
        }
        if ch == '-'
            && self.spec.flags
            && boundary
            && rest[1..]
                .chars()
                .next()
                .is_some_and(|next| next.is_alphabetic() || next == '-')
        {
            let end = rest[1..]
                .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_'))
                .map_or(rest.len(), |at| at + 1);
            self.push(i, i + end, TokenKind::Attribute);
            return i + end;
        }
        i + ch.len_utf8()
    }

    /// A string that starts at `at` (its opening delimiter) with rule `index`; the token starts
    /// at `token_start` (before a prefix). A JSON key becomes a property.
    fn string_token(&mut self, token_start: usize, index: usize, at: usize, raw: bool) -> usize {
        let before = self.out.len();
        let open = self.spec.strings[index].open.len();
        let end = self.string_body(token_start, index, at + open, raw);
        if self.spec.json_keys
            && self.state.mode == Mode::Normal
            && self.line[end..].trim_start().starts_with(':')
        {
            self.out.truncate(before);
            self.push(token_start, end, TokenKind::Property);
        }
        end
    }

    fn number_end(&self, i: usize) -> usize {
        let bytes = self.line.as_bytes();
        let len = bytes.len();
        let digits = |mut j: usize| {
            while j < len && (bytes[j].is_ascii_digit() || bytes[j] == b'_') {
                j += 1;
            }
            j
        };
        let mut j = i;
        if bytes[j] == b'0'
            && j + 1 < len
            && matches!(bytes[j + 1], b'x' | b'X' | b'b' | b'B' | b'o' | b'O')
        {
            j += 2;
            while j < len && (bytes[j].is_ascii_hexdigit() || bytes[j] == b'_') {
                j += 1;
            }
        } else if self.spec.date_numbers {
            while j < len
                && (bytes[j].is_ascii_alphanumeric()
                    || matches!(bytes[j], b'_' | b'-' | b':' | b'+' | b'.'))
            {
                j += 1;
            }
        } else {
            j = digits(j);
            if j + 1 < len && bytes[j] == b'.' && bytes[j + 1].is_ascii_digit() {
                j = digits(j + 1);
            }
            if j < len && matches!(bytes[j], b'e' | b'E') {
                let mut k = j + 1;
                if k < len && matches!(bytes[k], b'+' | b'-') {
                    k += 1;
                }
                if k < len && bytes[k].is_ascii_digit() {
                    j = digits(k);
                }
            }
        }
        while j < len && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        j
    }

    /// An identifier, keyword, prefixed string or call at `i`.
    fn word(&mut self, i: usize) -> usize {
        let line = self.line;
        let end = self.ident_end(i);
        let word = &line[i..end];
        let next = line[end..].chars().next();
        if self.spec.rust_lexical
            && matches!(word, "r" | "br" | "cr")
            && matches!(next, Some('#' | '"'))
        {
            let hashes = line[end..].bytes().take_while(|&b| b == b'#').count();
            if line[end + hashes..].starts_with('"') {
                return self.raw_body(i, end + hashes + 1, hashes);
            }
        }
        if self.spec.rust_lexical && word == "b" && next == Some('\'') {
            self.push(i, end, TokenKind::String);
            return self.rust_quote(end);
        }
        if matches!(next, Some('"' | '\'' | '`'))
            && self
                .spec
                .prefixes
                .iter()
                .any(|prefix| prefix.eq_ignore_ascii_case(word))
            && let Some(index) = self
                .spec
                .strings
                .iter()
                .position(|rule| line[end..].starts_with(rule.open))
        {
            let raw = word.chars().any(|c| c.eq_ignore_ascii_case(&'r'));
            return self.string_token(i, index, end, raw);
        }
        let after = line[end..].trim_start_matches([' ', '\t']);
        let kind = self.classify(word, next, after);
        if kind != TokenKind::Plain {
            self.push(i, end, kind);
        }
        end
    }

    fn classify(&self, word: &str, next: Option<char>, after: &str) -> TokenKind {
        let spec = self.spec;
        let lowered;
        let key = if spec.case_insensitive {
            lowered = word.to_ascii_lowercase();
            lowered.as_str()
        } else {
            word
        };
        let has = |list: &[&str]| list.contains(&key);
        if has(spec.keywords) {
            TokenKind::Keyword
        } else if has(spec.constants) {
            TokenKind::Constant
        } else if has(spec.types) {
            TokenKind::Type
        } else if has(spec.builtins)
            || (spec.bang_macro && next == Some('!') && !after.starts_with("!="))
            || (spec.verb_noun && is_verb_noun(word))
            || (spec.call_is_function && next == Some('('))
        {
            TokenKind::Function
        } else if spec.capitalized_types
            && word.starts_with(char::is_uppercase)
            && word.chars().any(char::is_lowercase)
        {
            TokenKind::Type
        } else if spec.caps_constants
            && word.len() > 1
            && word.starts_with(char::is_uppercase)
            && word
                .chars()
                .all(|c| c.is_uppercase() || c.is_ascii_digit() || c == '_')
        {
            TokenKind::Constant
        } else {
            TokenKind::Plain
        }
    }

    fn rust_attribute(&mut self, i: usize) -> usize {
        let line = self.line;
        let mut depth = 0usize;
        let mut end = line.len();
        for (at, ch) in line[i..].char_indices() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = i + at + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        self.push(i, end, TokenKind::Attribute);
        end
    }

    /// `#include <x>` and friends: the directive is a keyword, `<x>` a string.
    fn directive(&mut self, i: usize) -> usize {
        let line = self.line;
        let name_start = i + 1 + (line[i + 1..].len() - line[i + 1..].trim_start().len());
        let name_end = name_start
            + line[name_start..]
                .find(|c: char| !c.is_alphanumeric())
                .unwrap_or(line.len() - name_start);
        self.push(i, name_end, TokenKind::Keyword);
        if matches!(&line[name_start..name_end], "include" | "import") {
            let tail = &line[name_end..];
            let skipped = tail.len() - tail.trim_start().len();
            let angle = name_end + skipped;
            if line[angle..].starts_with('<')
                && let Some(close) = line[angle..].find('>')
            {
                self.push(angle, angle + close + 1, TokenKind::String);
                return angle + close + 1;
            }
        }
        name_end
    }

    /// A Rust `'`: a character literal (`'a'`, `'\n'`) or a lifetime (`'a`).
    fn rust_quote(&mut self, i: usize) -> usize {
        let line = self.line;
        let body = &line[i + 1..];
        if body.starts_with('\\') {
            let len = escape_len(body);
            if body[len..].starts_with('\'') {
                self.push(i, i + 1, TokenKind::String);
                self.push(i + 1, i + 1 + len, TokenKind::Escape);
                self.push(i + 1 + len, i + 2 + len, TokenKind::String);
                return i + 2 + len;
            }
            return i + 1;
        }
        let Some(first) = body.chars().next() else {
            return i + 1;
        };
        let after = i + 1 + first.len_utf8();
        if line[after..].starts_with('\'') {
            self.push(i, after + 1, TokenKind::String);
            return after + 1;
        }
        if first.is_alphabetic() || first == '_' {
            let end = self.ident_end(i + 1);
            self.push(i, end, TokenKind::Type);
            return end;
        }
        i + 1
    }
}

/// `Get-ChildItem`: capitalized words joined by one hyphen.
fn is_verb_noun(word: &str) -> bool {
    let mut parts = word.split('-');
    let (Some(verb), Some(noun), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let capitalized = |part: &str| {
        part.starts_with(char::is_uppercase) && part.chars().all(char::is_alphanumeric)
    };
    capitalized(verb) && capitalized(noun)
}

/// Length of the YAML mapping key at the start of `text` (including quotes), if `text` is
/// `key: value` or `key:`.
fn yaml_key_end(text: &str) -> Option<usize> {
    let first = text.chars().next()?;
    if "#[{&*!|>%@`".contains(first) {
        return None;
    }
    let key_len = if first == '"' || first == '\'' {
        text[1..].find(first)? + 2
    } else {
        let mut found = None;
        for (at, ch) in text.char_indices() {
            if ch == ':'
                && text[at + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
            {
                found = Some(at);
                break;
            }
            if ch == '#' && text[..at].ends_with(' ') {
                break;
            }
        }
        found?
    };
    let after = text[key_len..].strip_prefix(':')?;
    after
        .chars()
        .next()
        .is_none_or(char::is_whitespace)
        .then_some(key_len)
}

/// Length of the TOML key (bare or quoted, dotted) at the start of `text` if it is followed by
/// `=`.
fn toml_key_end(text: &str) -> Option<usize> {
    let mut len = 0;
    let mut rest = text;
    loop {
        let part = if let Some(inner) = rest.strip_prefix('"').or_else(|| rest.strip_prefix('\'')) {
            inner.find(rest.chars().next()?)? + 2
        } else {
            rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(rest.len())
        };
        if part == 0 {
            return None;
        }
        len += part;
        rest = &text[len..];
        let spaced = rest.trim_start();
        if spaced.starts_with('=') {
            return Some(len);
        }
        rest = rest.strip_prefix('.')?;
        len += 1;
    }
}
