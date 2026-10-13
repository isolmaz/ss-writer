//! JSON and JSON with comments: tokenize, validate, re-emit the whitespace between tokens.
//!
//! Dialect: RFC 8259 plus `//` and `/* */` comments and trailing commas. Strings, numbers,
//! literals and comments are copied byte for byte; only the whitespace between tokens is
//! regenerated, as a function of the token kinds, the nesting depth and — for comments and
//! blank lines — whether the source had a line break at that place. The scanner and the
//! printer are iterative, so nesting depth cannot overflow the stack.
//!
//! Layout rules:
//!
//! * Entries of an object or array sit on their own line, one indent unit deeper than the
//!   brackets; `{}` and `[]` stay collapsed. `"key": value` and the comma ends the line.
//! * A comment that follows an opening bracket or a comma on the same line stays on that line
//!   when it ends the line; a block comment that is followed by its entry on the same line
//!   moves to the entry's line. Any other comment keeps the line structure of the source.
//!   A `//` comment always ends its line; a comment inside an empty container expands it.
//! * Blank lines between entries and comments collapse to at most one; none after an opening
//!   bracket or before a closing one.

use super::{
    EolCounts, FormatIndent, FormatOutcome, Refusal, RefusalReason, bom_len, find, finish,
    line_ending,
};

/// Containers nested deeper than this are refused.
const MAX_DEPTH: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    ObjectOpen,
    ObjectClose,
    ArrayOpen,
    ArrayClose,
    Colon,
    Comma,
    Str,
    Number,
    Literal,
    LineComment,
    BlockComment,
}

impl Kind {
    const fn is_open(self) -> bool {
        matches!(self, Self::ObjectOpen | Self::ArrayOpen)
    }

    const fn is_close(self) -> bool {
        matches!(self, Self::ObjectClose | Self::ArrayClose)
    }

    const fn is_comment(self) -> bool {
        matches!(self, Self::LineComment | Self::BlockComment)
    }
}

#[derive(Clone, Copy, Debug)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
    /// Line breaks in the whitespace in front of the token. The terminator of a `//` comment
    /// belongs to the gap of the token after it.
    breaks: usize,
}

struct Lexed {
    tokens: Vec<Token>,
    /// Line breaks after the last token.
    trailing_breaks: usize,
    /// The line breaks between tokens, by spelling.
    gaps: EolCounts,
}

pub(super) fn format(text: &str, indent: FormatIndent) -> FormatOutcome {
    match run(text, indent) {
        Ok(formatted) => finish(text, formatted),
        Err(refusal) => FormatOutcome::Refused(refusal),
    }
}

fn run(text: &str, indent: FormatIndent) -> Result<String, Refusal> {
    let bom = bom_len(text);
    let lexed = lex(text, bom)?;
    validate(&lexed.tokens, text.len())?;
    let eol = line_ending(lexed.gaps, text);
    Ok(emit(text, bom, &lexed, eol, indent.unit()))
}

// ------------------------------------------------------------------------------------ lexer

fn lex(text: &str, bom: usize) -> Result<Lexed, Refusal> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut gaps = EolCounts::default();
    let mut i = bom;
    loop {
        let gap_start = i;
        while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        let breaks = gaps.count(&bytes[gap_start..i]);
        let Some(&first) = bytes.get(i) else {
            return Ok(Lexed {
                tokens,
                trailing_breaks: breaks,
                gaps,
            });
        };
        let start = i;
        let kind = match first {
            b'{' => {
                i += 1;
                Kind::ObjectOpen
            }
            b'}' => {
                i += 1;
                Kind::ObjectClose
            }
            b'[' => {
                i += 1;
                Kind::ArrayOpen
            }
            b']' => {
                i += 1;
                Kind::ArrayClose
            }
            b':' => {
                i += 1;
                Kind::Colon
            }
            b',' => {
                i += 1;
                Kind::Comma
            }
            b'"' => {
                i = scan_string(bytes, start)?;
                Kind::Str
            }
            b'-' | b'0'..=b'9' => {
                i = scan_number(bytes, start)?;
                Kind::Number
            }
            b't' | b'f' | b'n' => {
                i = scan_literal(bytes, start)?;
                Kind::Literal
            }
            b'/' => match bytes.get(i + 1) {
                Some(b'/') => {
                    i += 2;
                    while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                        i += 1;
                    }
                    Kind::LineComment
                }
                Some(b'*') => {
                    let Some(close) = find(bytes, i + 2, b"*/") else {
                        return Err(Refusal::at(RefusalReason::UnterminatedComment, start));
                    };
                    i = close + 2;
                    Kind::BlockComment
                }
                _ => return Err(Refusal::at(RefusalReason::UnexpectedCharacter, i)),
            },
            _ => return Err(Refusal::at(RefusalReason::UnexpectedCharacter, i)),
        };
        tokens.push(Token {
            kind,
            start,
            end: i,
            breaks,
        });
    }
}

/// End of the string that starts at `start` (the opening quote).
fn scan_string(bytes: &[u8], start: usize) -> Result<usize, Refusal> {
    let mut i = start + 1;
    loop {
        match bytes.get(i) {
            None => return Err(Refusal::at(RefusalReason::UnterminatedString, start)),
            Some(b'"') => return Ok(i + 1),
            Some(b'\\') => match bytes.get(i + 1) {
                Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => i += 2,
                Some(b'u') => {
                    let hex = bytes
                        .get(i + 2..i + 6)
                        .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit));
                    if !hex {
                        return Err(Refusal::at(RefusalReason::InvalidEscape, i));
                    }
                    i += 6;
                }
                None => return Err(Refusal::at(RefusalReason::UnterminatedString, start)),
                Some(_) => return Err(Refusal::at(RefusalReason::InvalidEscape, i)),
            },
            Some(&byte) if byte < 0x20 => {
                return Err(Refusal::at(RefusalReason::ControlCharacter, i));
            }
            Some(_) => i += 1,
        }
    }
}

fn skip_digits(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    i
}

/// End of the number that starts at `start`: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
fn scan_number(bytes: &[u8], start: usize) -> Result<usize, Refusal> {
    let invalid = |at: usize| Refusal::at(RefusalReason::InvalidNumber, at);
    let mut i = start;
    if bytes[i] == b'-' {
        i += 1;
    }
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => i = skip_digits(bytes, i),
        _ => return Err(invalid(i)),
    }
    if bytes.get(i) == Some(&b'.') {
        let digits = skip_digits(bytes, i + 1);
        if digits == i + 1 {
            return Err(invalid(digits));
        }
        i = digits;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut exponent = i + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let digits = skip_digits(bytes, exponent);
        if digits == exponent {
            return Err(invalid(digits));
        }
        i = digits;
    }
    Ok(i)
}

fn scan_literal(bytes: &[u8], start: usize) -> Result<usize, Refusal> {
    const LITERALS: [&[u8]; 3] = [b"true", b"false", b"null"];
    for literal in LITERALS {
        if bytes[start..].starts_with(literal) {
            return Ok(start + literal.len());
        }
    }
    Err(Refusal::at(RefusalReason::UnexpectedCharacter, start))
}

// -------------------------------------------------------------------------------- validation

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the document value.
    Start,
    /// In an object after `{` or `,`: a key, or `}`.
    Key,
    /// After a key.
    Colon,
    /// After `:`: a value.
    Value,
    /// In an array after `[` or `,`: a value, or `]`.
    Item,
    /// After a value inside a container: `,` or the closing bracket.
    After,
    /// After the document value.
    End,
}

/// Checks the token sequence against the grammar (comments are allowed between any tokens).
fn validate(tokens: &[Token], text_len: usize) -> Result<(), Refusal> {
    // `true` for an open object, `false` for an open array.
    let mut stack: Vec<bool> = Vec::new();
    let mut state = State::Start;
    for token in tokens {
        if token.kind.is_comment() {
            continue;
        }
        if state == State::End {
            return Err(Refusal::at(RefusalReason::TrailingContent, token.start));
        }
        state = match (token.kind, state) {
            (Kind::ObjectOpen | Kind::ArrayOpen, State::Start | State::Value | State::Item) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(Refusal::at(RefusalReason::NestingTooDeep, token.start));
                }
                let object = token.kind == Kind::ObjectOpen;
                stack.push(object);
                if object { State::Key } else { State::Item }
            }
            (Kind::Str, State::Key) => State::Colon,
            (
                Kind::Str | Kind::Number | Kind::Literal,
                State::Start | State::Value | State::Item,
            ) => value_end(&stack),
            // `Key` and `Item` are the states right after an opening bracket or a comma, so
            // a bracket closes there (an empty container or a trailing comma) as well as after
            // a value — but only the bracket that matches the open container.
            (Kind::ObjectClose, State::Key | State::After) if stack.last() == Some(&true) => {
                stack.pop();
                value_end(&stack)
            }
            (Kind::ArrayClose, State::Item | State::After) if stack.last() == Some(&false) => {
                stack.pop();
                value_end(&stack)
            }
            (Kind::Colon, State::Colon) => State::Value,
            (Kind::Comma, State::After) => {
                if stack.last() == Some(&true) {
                    State::Key
                } else {
                    State::Item
                }
            }
            _ => return Err(Refusal::at(RefusalReason::UnexpectedToken, token.start)),
        };
    }
    match state {
        State::End => Ok(()),
        State::Start => Err(Refusal::at(RefusalReason::EmptyDocument, text_len)),
        _ => Err(Refusal::at(RefusalReason::UnexpectedEnd, text_len)),
    }
}

/// The state after a complete value, given the containers that are still open.
fn value_end(stack: &[bool]) -> State {
    if stack.is_empty() {
        State::End
    } else {
        State::After
    }
}

// ------------------------------------------------------------------------------------ emit

#[derive(Clone, Copy, PartialEq, Eq)]
enum Gap {
    /// Nothing between the two tokens.
    Join,
    /// One space.
    Space,
    /// A line break (two for a blank line) and the indent.
    Break { blank: bool },
}

fn emit(text: &str, bom: usize, lexed: &Lexed, eol: &str, unit: &str) -> String {
    let tokens = &lexed.tokens;
    let mut out = String::with_capacity(text.len() + text.len() / 2);
    out.push_str(&text[..bom]);
    let mut depth = 0usize;
    let mut last_real: Option<Kind> = None;
    for (index, token) in tokens.iter().enumerate() {
        if token.kind.is_close() {
            depth = depth.saturating_sub(1);
        }
        if index > 0 {
            match gap(tokens, index, last_real) {
                Gap::Join => {}
                Gap::Space => out.push(' '),
                Gap::Break { blank } => {
                    out.push_str(eol);
                    if blank {
                        out.push_str(eol);
                    }
                    for _ in 0..depth {
                        out.push_str(unit);
                    }
                }
            }
        }
        out.push_str(&text[token.start..token.end]);
        if token.kind.is_open() {
            depth += 1;
        }
        if !token.kind.is_comment() {
            last_real = Some(token.kind);
        }
    }
    if lexed.trailing_breaks > 0 {
        out.push_str(eol);
    }
    out
}

/// The whitespace between `tokens[x - 1]` and `tokens[x]`. `last_real` is the last token
/// before `x` that is not a comment.
fn gap(tokens: &[Token], x: usize, last_real: Option<Kind>) -> Gap {
    let a = tokens[x - 1].kind;
    let b = tokens[x].kind;
    let breaks = tokens[x].breaks;
    // A blank line survives between entries and comments, never after an opening bracket or
    // inside an entry.
    let blank = breaks >= 2 && !a.is_open() && last_real != Some(Kind::Colon);
    // Commas and colons hug what precedes them, unless a `//` comment ended that line.
    if matches!(b, Kind::Comma | Kind::Colon) {
        return if a == Kind::LineComment {
            Gap::Break { blank: false }
        } else {
            Gap::Join
        };
    }
    // Closing brackets sit on their own line, except in an empty container.
    if b.is_close() {
        return if a.is_open() {
            Gap::Join
        } else {
            Gap::Break { blank: false }
        };
    }
    if a == Kind::LineComment {
        return Gap::Break { blank };
    }
    // After an opening bracket or a comma the next entry starts a line. A comment that ends
    // the line stays on the line it started on.
    if a.is_open() || a == Kind::Comma {
        let trails = breaks == 0
            && match b {
                Kind::LineComment => true,
                Kind::BlockComment => ends_line(tokens, x),
                _ => false,
            };
        return if trails {
            Gap::Space
        } else {
            Gap::Break { blank }
        };
    }
    // Everything after a colon stays on its line.
    if a == Kind::Colon {
        return Gap::Space;
    }
    // Elsewhere (after a key, a value or a block comment) the source decides.
    let inside_entry = last_real == Some(Kind::Colon) && !b.is_comment();
    if breaks > 0 && !inside_entry {
        Gap::Break { blank }
    } else {
        Gap::Space
    }
}

/// Whether the run of block comments that starts at `tokens[x]` ends its line — by a line
/// break, a `//` comment or a closing bracket — before another value begins.
fn ends_line(tokens: &[Token], x: usize) -> bool {
    for token in &tokens[x + 1..] {
        if token.breaks > 0 || token.kind.is_close() || token.kind == Kind::LineComment {
            return true;
        }
        if token.kind != Kind::BlockComment {
            return false;
        }
    }
    true
}
