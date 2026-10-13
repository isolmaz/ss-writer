//! Whole-document formatters for JSON (with comments), XML and HTML.
//!
//! The engine behind the manual "Format document" command. Every formatter is a
//! **token-preserving, whitespace-only re-emitter**: it tokenizes and validates the document,
//! copies the bytes of every token verbatim and regenerates only the whitespace between tokens.
//! Nothing a normalizing formatter rewrites is ever touched: number spelling, string escapes,
//! key order and duplicate keys, comments, trailing commas, attribute order, quote style,
//! empty-element syntax, CDATA, processing instructions. A document that does not parse is
//! refused and left alone — never edited partially.
//!
//! * [`format_json`] accepts RFC 8259 JSON plus `//` and `/* */` comments and trailing commas.
//!   Objects and arrays expand one entry per line; `{}` and `[]` stay collapsed.
//! * [`format_xml`] and [`format_html`] re-indent element structure only. An element is
//!   re-laid out when it holds nothing but child elements, comments and processing
//!   instructions and the whitespace between them cannot be content; everything else stays
//!   byte for byte — text, mixed content, `<pre>`, `<textarea>`, `<script>`, `<style>`, CDATA,
//!   comments, processing instructions, DOCTYPE. HTML additionally keeps every container that
//!   has an inline-level child, because the whitespace between inline elements is significant.
//!
//! All formatters keep the byte order mark, the dominant line ending and whether the document
//! ends with a line break, and formatting the output again changes nothing.

mod json;
mod markup;

use std::fmt;

/// One indent level of the formatted output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FormatIndent {
    /// Two spaces per level.
    #[default]
    Two,
    /// Four spaces per level.
    Four,
    /// One tab per level.
    Tab,
}

impl FormatIndent {
    /// The text of one indent level.
    #[must_use]
    pub const fn unit(self) -> &'static str {
        match self {
            Self::Two => "  ",
            Self::Four => "    ",
            Self::Tab => "\t",
        }
    }
}

/// What a formatter did with a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormatOutcome {
    /// The reformatted document; differs from the input.
    Formatted(String),
    /// The input already is what the formatter would produce.
    AlreadyFormatted,
    /// The input is not valid or not supported; nothing was changed.
    Refused(Refusal),
}

/// Why a document was left alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub reason: RefusalReason,
    /// Byte offset into the document passed to the formatter (a leading byte order mark
    /// counts), where it is known.
    pub offset: Option<usize>,
}

impl Refusal {
    const fn new(reason: RefusalReason) -> Self {
        Self {
            reason,
            offset: None,
        }
    }

    const fn at(reason: RefusalReason, offset: usize) -> Self {
        Self {
            reason,
            offset: Some(offset),
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.offset {
            Some(offset) => write!(f, "{} (at byte {offset})", self.reason),
            None => fmt::Display::fmt(&self.reason, f),
        }
    }
}

/// The reason of a [`Refusal`]; [`RefusalReason::as_str`] is a short English description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// No value (JSON) or no element (XML, HTML) in the document.
    EmptyDocument,
    /// A character that starts no token.
    UnexpectedCharacter,
    /// A token where the grammar does not allow it (missing comma, key without value, …).
    UnexpectedToken,
    /// The document ends inside a value, a container or a tag.
    UnexpectedEnd,
    /// A second JSON value after the document value.
    TrailingContent,
    /// A number that does not follow the JSON grammar.
    InvalidNumber,
    /// A backslash sequence in a JSON string that is not an escape.
    InvalidEscape,
    /// A raw control character in a JSON string.
    ControlCharacter,
    /// A string without closing quote.
    UnterminatedString,
    /// A comment without its closing delimiter.
    UnterminatedComment,
    /// Containers nested deeper than the formatter supports.
    NestingTooDeep,
    /// An end tag that does not close the innermost open element.
    MismatchedTag,
    /// An element that is never closed.
    UnclosedTag,
    /// A tag that does not follow the markup grammar.
    MalformedTag,
    /// An attribute value without closing quote.
    UnterminatedAttribute,
    /// A CDATA section, processing instruction, DOCTYPE or raw-text element without its end.
    UnterminatedConstruct,
    /// Text or a second element outside the root element of an XML document.
    ContentOutsideRoot,
    /// Markup the formatter cannot lay out without risking its meaning (template tags, a
    /// double-escaped script, a declaration XML does not have).
    UnsupportedSyntax,
    /// The formatted text lost or gained something besides whitespace; a bug guard.
    Inconsistent,
}

impl RefusalReason {
    /// A short English description.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyDocument => "the document is empty",
            Self::UnexpectedCharacter => "unexpected character",
            Self::UnexpectedToken => "unexpected token",
            Self::UnexpectedEnd => "the document ends unexpectedly",
            Self::TrailingContent => "content after the end of the document",
            Self::InvalidNumber => "invalid number",
            Self::InvalidEscape => "invalid escape sequence in a string",
            Self::ControlCharacter => "unescaped control character in a string",
            Self::UnterminatedString => "unterminated string",
            Self::UnterminatedComment => "unterminated comment",
            Self::NestingTooDeep => "nesting is too deep",
            Self::MismatchedTag => "end tag does not match the open element",
            Self::UnclosedTag => "element is never closed",
            Self::MalformedTag => "malformed tag",
            Self::UnterminatedAttribute => "unterminated attribute value",
            Self::UnterminatedConstruct => "unterminated markup construct",
            Self::ContentOutsideRoot => "content outside the root element",
            Self::UnsupportedSyntax => "unsupported syntax",
            Self::Inconsistent => "internal consistency check failed",
        }
    }
}

impl fmt::Display for RefusalReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Formats JSON, or JSON with comments and trailing commas.
#[must_use]
pub fn format_json(text: &str, indent: FormatIndent) -> FormatOutcome {
    json::format(text, indent)
}

/// Re-indents an XML document. The document must be well formed with exactly one root element.
#[must_use]
pub fn format_xml(text: &str, indent: FormatIndent) -> FormatOutcome {
    markup::format(text, indent, markup::Dialect::Xml)
}

/// Re-indents an HTML document or fragment. Every non-void element needs its end tag; markup
/// that relies on omitted optional end tags is refused as unbalanced.
#[must_use]
pub fn format_html(text: &str, indent: FormatIndent) -> FormatOutcome {
    markup::format(text, indent, markup::Dialect::Html)
}

// ------------------------------------------------------------------------------------ shared

/// Packages a formatter's result: guards that only whitespace changed, and tells a changed
/// document from an unchanged one.
fn finish(text: &str, formatted: String) -> FormatOutcome {
    if !significant(text).eq(significant(&formatted)) {
        return FormatOutcome::Refused(Refusal::new(RefusalReason::Inconsistent));
    }
    if formatted == text {
        FormatOutcome::AlreadyFormatted
    } else {
        FormatOutcome::Formatted(formatted)
    }
}

/// The bytes of `text` that are not ASCII whitespace.
fn significant(text: &str) -> impl Iterator<Item = u8> + '_ {
    text.bytes().filter(|b| !b.is_ascii_whitespace())
}

/// Counts of the three line break spellings.
#[derive(Clone, Copy, Debug, Default)]
struct EolCounts {
    lf: usize,
    crlf: usize,
    cr: usize,
}

impl EolCounts {
    /// Adds the line breaks of `bytes` (`\r\n` is one break) and returns how many there were.
    fn count(&mut self, bytes: &[u8]) -> usize {
        let mut breaks = 0;
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    self.lf += 1;
                    breaks += 1;
                    i += 1;
                }
                b'\r' => {
                    if bytes.get(i + 1) == Some(&b'\n') {
                        self.crlf += 1;
                        i += 2;
                    } else {
                        self.cr += 1;
                        i += 1;
                    }
                    breaks += 1;
                }
                _ => i += 1,
            }
        }
        breaks
    }

    const fn total(self) -> usize {
        self.lf + self.crlf + self.cr
    }

    /// The most frequent spelling; LF on ties, then CRLF before a lone CR.
    const fn dominant(self) -> &'static str {
        let mut best = ("\n", self.lf);
        if self.crlf > best.1 {
            best = ("\r\n", self.crlf);
        }
        if self.cr > best.1 {
            best = ("\r", self.cr);
        }
        best.0
    }
}

/// The line ending for generated breaks: the dominant one among the breaks the formatter
/// rewrites (`layout`), or among all breaks of the document when there are none. Counting only
/// the rewritten breaks keeps the choice stable when the output is formatted again.
fn line_ending(layout: EolCounts, text: &str) -> &'static str {
    if layout.total() > 0 {
        return layout.dominant();
    }
    let mut all = EolCounts::default();
    all.count(text.as_bytes());
    all.dominant()
}

/// Byte offset of the first occurrence of `needle` in `bytes` at or after `from`.
fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| position + from)
}

/// Whether `bytes` contains `needle`, ignoring ASCII case.
fn contains_ci(bytes: &[u8], needle: &[u8]) -> bool {
    bytes
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Length of the byte order mark at the start of `text` (0 or 3).
fn bom_len(text: &str) -> usize {
    if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    }
}
