//! Text encodings, BOMs and line endings (spec §4.2).
//!
//! Detection order: BOM → UTF-16/32 null-byte heuristic → valid UTF-8 → `chardetng`
//! (legacy code pages incl. Windows-1254, optionally biased by a TLD hint derived from the
//! Windows locale). Documents are decoded to `\n`-only [`String`]s; the original encoding, BOM
//! and line-ending style are kept in a [`TextFormat`] and restored byte-for-byte by [`encode`].

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, GB18030, GBK, REPLACEMENT, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};
use serde::{Deserialize, Serialize};
use windows::Win32::Globalization::GetUserDefaultLocaleName;

/// Number of unmappable characters listed by an [`EncodeError`].
pub const MAX_REPORTED_UNMAPPABLE: usize = 10;

const BOM_UTF8: [u8; 3] = [0xEF, 0xBB, 0xBF];
const BOM_UTF16LE: [u8; 2] = [0xFF, 0xFE];
const BOM_UTF16BE: [u8; 2] = [0xFE, 0xFF];
const BOM_UTF32LE: [u8; 4] = [0xFF, 0xFE, 0x00, 0x00];
const BOM_UTF32BE: [u8; 4] = [0x00, 0x00, 0xFE, 0xFF];

/// A character encoding a document can be stored in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum TextEncoding {
    /// UTF-8.
    Utf8,
    /// UTF-16 little endian.
    Utf16Le,
    /// UTF-16 big endian.
    Utf16Be,
    /// UTF-32 little endian.
    Utf32Le,
    /// UTF-32 big endian.
    Utf32Be,
    /// A legacy code page handled by `encoding_rs` (never UTF-8/UTF-16/replacement).
    Legacy(&'static Encoding),
}

impl TextEncoding {
    /// Stable display / persistence name (`UTF-8`, `UTF-16LE`, `windows-1254`, …).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf16Le => "UTF-16LE",
            Self::Utf16Be => "UTF-16BE",
            Self::Utf32Le => "UTF-32LE",
            Self::Utf32Be => "UTF-32BE",
            Self::Legacy(encoding) => encoding.name(),
        }
    }

    /// Parses a name or WHATWG label (case-insensitive). `None` for unknown labels and for
    /// encodings that cannot be written (`replacement`).
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        let lower = label.trim().to_ascii_lowercase();
        match lower.as_str() {
            "utf-8" | "utf8" => Some(Self::Utf8),
            "utf-16" | "utf-16le" => Some(Self::Utf16Le),
            "utf-16be" => Some(Self::Utf16Be),
            "utf-32" | "utf-32le" => Some(Self::Utf32Le),
            "utf-32be" => Some(Self::Utf32Be),
            _ => {
                let encoding = Encoding::for_label(lower.as_bytes())?;
                if encoding == REPLACEMENT {
                    None
                } else if encoding == UTF_8 {
                    Some(Self::Utf8)
                } else if encoding == UTF_16LE {
                    Some(Self::Utf16Le)
                } else if encoding == UTF_16BE {
                    Some(Self::Utf16Be)
                } else {
                    Some(Self::Legacy(encoding))
                }
            }
        }
    }

    /// Encodings offered by the status-bar "reopen with / save with encoding" menus.
    #[must_use]
    pub fn selectable() -> Vec<Self> {
        let mut all = vec![
            Self::Utf8,
            Self::Utf16Le,
            Self::Utf16Be,
            Self::Utf32Le,
            Self::Utf32Be,
        ];
        let labels = [
            "windows-1250",
            "windows-1251",
            "windows-1252",
            "windows-1253",
            "windows-1254",
            "windows-1255",
            "windows-1256",
            "windows-1257",
            "windows-1258",
            "iso-8859-2",
            "iso-8859-5",
            "iso-8859-7",
            "iso-8859-15",
            "koi8-r",
            "koi8-u",
            "shift_jis",
            "euc-jp",
            "euc-kr",
            "gbk",
            "gb18030",
            "big5",
        ];
        all.extend(labels.iter().filter_map(|label| Self::from_label(label)));
        all
    }

    /// Whether this encoding has a byte-order mark that can be stored in the file.
    #[must_use]
    pub fn supports_bom(self) -> bool {
        !matches!(self, Self::Legacy(_))
    }

    fn bom(self) -> &'static [u8] {
        match self {
            Self::Utf8 => &BOM_UTF8,
            Self::Utf16Le => &BOM_UTF16LE,
            Self::Utf16Be => &BOM_UTF16BE,
            Self::Utf32Le => &BOM_UTF32LE,
            Self::Utf32Be => &BOM_UTF32BE,
            Self::Legacy(_) => &[],
        }
    }
}

impl From<TextEncoding> for String {
    fn from(value: TextEncoding) -> Self {
        value.name().to_owned()
    }
}

impl TryFrom<String> for TextEncoding {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_label(&value).ok_or_else(|| format!("unknown encoding `{value}`"))
    }
}

/// Line-ending style of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineEnding {
    /// `\n`.
    #[default]
    Lf,
    /// `\r\n`.
    Crlf,
    /// `\r`.
    Cr,
}

impl LineEnding {
    /// The byte sequence as text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
            Self::Cr => "\r",
        }
    }

    /// Label shown in the status bar.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Lf => "LF",
            Self::Crlf => "CRLF",
            Self::Cr => "CR",
        }
    }
}

/// How a document was stored on disk; restored on save.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextFormat {
    /// Character encoding.
    pub encoding: TextEncoding,
    /// A byte-order mark was present and is written back.
    pub bom: bool,
    /// Majority line-ending style.
    pub line_ending: LineEnding,
    /// The file used more than one line-ending style (normalized to the majority style).
    pub had_mixed_endings: bool,
}

impl Default for TextFormat {
    fn default() -> Self {
        Self {
            encoding: TextEncoding::Utf8,
            bom: false,
            line_ending: LineEnding::Lf,
            had_mixed_endings: false,
        }
    }
}

/// Result of decoding a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decoded {
    /// Text with `\n` line endings.
    pub text: String,
    /// Original format.
    pub format: TextFormat,
    /// Malformed input was replaced by U+FFFD: saving would not reproduce the original bytes.
    pub lossy: bool,
}

/// One character that cannot be written in the target encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnmappableChar {
    /// Byte offset in the document text.
    pub offset: usize,
    /// 1-based line.
    pub line: usize,
    /// 1-based column in characters.
    pub column: usize,
    /// The character.
    pub ch: char,
}

/// Saving failed because of the target encoding.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EncodeError {
    /// Characters outside the repertoire of the encoding.
    #[error("{total} character(s) cannot be represented in {encoding}")]
    Unmappable {
        /// Target encoding name.
        encoding: &'static str,
        /// Number of unmappable characters in the whole text.
        total: usize,
        /// The first [`MAX_REPORTED_UNMAPPABLE`] positions.
        first: Vec<UnmappableChar>,
    },
}

/// Detects the encoding of `bytes`. `tld` biases the legacy-code-page guess (`"tr"` for a
/// Turkish locale, see [`tld_hint_for_locale`]).
#[must_use]
pub fn detect(bytes: &[u8], tld: Option<&str>) -> TextEncoding {
    if let Some(encoding) = bom_encoding(bytes) {
        return encoding;
    }
    if let Some(encoding) = sniff_wide(bytes) {
        return encoding;
    }
    if std::str::from_utf8(bytes).is_ok() {
        return TextEncoding::Utf8;
    }
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let mut guess = detector.guess(tld.map(str::as_bytes), Utf8Detection::Deny);
    if guess == GBK {
        // GB18030 decodes the same bytes but can also encode the four-byte forms.
        guess = GB18030;
    }
    lossless_legacy(guess, bytes)
}

/// `guess` when it decodes `bytes` without errors and encodes the text back to the same bytes;
/// otherwise (malformed input or a many-to-one code page) Windows-1252, which maps every byte,
/// so decoding and re-encoding is lossless.
fn lossless_legacy(guess: &'static Encoding, bytes: &[u8]) -> TextEncoding {
    let exact = guess
        .decode_without_bom_handling_and_without_replacement(bytes)
        .is_some_and(|text| guess.encode(&text).0.as_ref() == bytes);
    TextEncoding::Legacy(if exact { guess } else { WINDOWS_1252 })
}

/// Number of leading bytes [`looks_binary`] inspects: what a caller reads when it classifies a
/// file without reading it whole.
pub const BINARY_SNIFF_LEN: usize = 8192;

/// Whether `bytes` look like a binary file rather than text. A UTF-8, UTF-16 or UTF-32 BOM
/// means text (UTF-16/32 text is full of NUL bytes); otherwise a NUL byte among the first 8192
/// bytes means binary, unless [`detect`] takes the bytes for UTF-16 or UTF-32 text without a
/// BOM, whatever the file is called (the text itself has no NUL character: a zero-filled
/// start, as a disk image has, is no text); everything else, an empty file included, is text.
#[must_use]
pub fn looks_binary(bytes: &[u8]) -> bool {
    let window = &bytes[..bytes.len().min(BINARY_SNIFF_LEN)];
    window.contains(&0) && bom_encoding(bytes).is_none() && !is_wide_text(window)
}

/// Whether `window` (the start of a file, without a BOM) is UTF-16 or UTF-32 text: [`detect`]
/// sniffs a wide encoding and the text it decodes to holds no NUL character.
fn is_wide_text(window: &[u8]) -> bool {
    let wide = |bytes: &[u8]| {
        sniff_wide(bytes).is_some_and(|encoding| !decode_as(bytes, encoding).text.contains('\0'))
    };
    // A file longer than the window is cut at its end, maybe inside a surrogate pair: the text
    // before that counts.
    wide(window) || (window.len() == BINARY_SNIFF_LEN && wide(&window[..BINARY_SNIFF_LEN - 2]))
}

/// Detects the encoding and decodes `bytes` (BOM stripped, line endings normalized).
#[must_use]
pub fn decode(bytes: &[u8], tld: Option<&str>) -> Decoded {
    decode_as(bytes, detect(bytes, tld))
}

/// Decodes `bytes` as `encoding` ("reopen with encoding"). A BOM matching the encoding is
/// stripped and remembered in the format.
#[must_use]
pub fn decode_as(bytes: &[u8], encoding: TextEncoding) -> Decoded {
    let bom = encoding.bom();
    let has_bom = !bom.is_empty() && bytes.starts_with(bom);
    let body = if has_bom { &bytes[bom.len()..] } else { bytes };
    let (raw, lossy) = match encoding {
        TextEncoding::Utf8 => match String::from_utf8(body.to_vec()) {
            Ok(text) => (text, false),
            Err(_) => (String::from_utf8_lossy(body).into_owned(), true),
        },
        TextEncoding::Utf16Le => decode_utf16(body, false),
        TextEncoding::Utf16Be => decode_utf16(body, true),
        TextEncoding::Utf32Le => decode_utf32(body, false),
        TextEncoding::Utf32Be => decode_utf32(body, true),
        TextEncoding::Legacy(legacy) => {
            let (text, had_errors) = legacy.decode_without_bom_handling(body);
            (text.into_owned(), had_errors)
        }
    };
    let (text, line_ending, had_mixed_endings) = normalize_line_endings(raw);
    Decoded {
        text,
        format: TextFormat {
            encoding,
            bom: has_bom,
            line_ending,
            had_mixed_endings,
        },
        lossy,
    }
}

/// Encodes `text` (with `\n` line endings) as described by `format`.
///
/// # Errors
/// [`EncodeError::Unmappable`] when the encoding cannot represent some characters; the caller
/// offers to save as UTF-8 instead.
pub fn encode(text: &str, format: &TextFormat) -> Result<Vec<u8>, EncodeError> {
    let converted;
    let body = if format.line_ending == LineEnding::Lf {
        text
    } else {
        converted = text.replace('\n', format.line_ending.as_str());
        converted.as_str()
    };
    let mut out = Vec::with_capacity(body.len() + 4);
    if format.bom {
        out.extend_from_slice(format.encoding.bom());
    }
    match format.encoding {
        TextEncoding::Utf8 => out.extend_from_slice(body.as_bytes()),
        TextEncoding::Utf16Le => body
            .encode_utf16()
            .for_each(|unit| out.extend_from_slice(&unit.to_le_bytes())),
        TextEncoding::Utf16Be => body
            .encode_utf16()
            .for_each(|unit| out.extend_from_slice(&unit.to_be_bytes())),
        TextEncoding::Utf32Le => body
            .chars()
            .for_each(|c| out.extend_from_slice(&u32::from(c).to_le_bytes())),
        TextEncoding::Utf32Be => body
            .chars()
            .for_each(|c| out.extend_from_slice(&u32::from(c).to_be_bytes())),
        TextEncoding::Legacy(legacy) => {
            let (bytes, _, had_errors) = legacy.encode(body);
            if had_errors {
                return Err(unmappable(legacy, text));
            }
            out.extend_from_slice(&bytes);
        }
    }
    Ok(out)
}

/// Maps a Windows locale name (`tr-TR`, `zh-Hans-CN`, `en-GB`) to the top-level-domain hint
/// `chardetng` understands (`tr`, `cn`, `uk`). `None` when the name carries no usable tag.
#[must_use]
pub fn tld_hint_for_locale(locale: &str) -> Option<String> {
    let mut tags = locale.split('-');
    let language = tags.next().filter(|tag| tag.len() == 2);
    let region = tags.find(|tag| tag.len() == 2 && tag.bytes().all(|b| b.is_ascii_alphabetic()));
    let tld = region.or(language)?.to_ascii_lowercase();
    Some(if tld == "gb" { "uk".to_owned() } else { tld })
}

/// Name of the user's default Windows locale (`tr-TR`); empty when unavailable.
#[must_use]
pub fn system_locale_name() -> String {
    let mut buffer = [0u16; 85];
    // SAFETY: the slice is a valid writable buffer; the API writes at most `len` UTF-16 units
    // and returns the number written including the terminating NUL (0 on failure).
    let written = unsafe { GetUserDefaultLocaleName(&mut buffer) };
    let length = usize::try_from(written).unwrap_or(0).saturating_sub(1);
    String::from_utf16_lossy(&buffer[..length])
}

/// TLD hint for the user's Windows locale (see [`tld_hint_for_locale`]).
#[must_use]
pub fn system_tld_hint() -> Option<String> {
    tld_hint_for_locale(&system_locale_name())
}

fn bom_encoding(bytes: &[u8]) -> Option<TextEncoding> {
    [
        TextEncoding::Utf8,
        TextEncoding::Utf32Le,
        TextEncoding::Utf32Be,
        TextEncoding::Utf16Le,
        TextEncoding::Utf16Be,
    ]
    .into_iter()
    .find(|encoding| bytes.starts_with(encoding.bom()))
}

/// UTF-16/32 without BOM: text in these encodings is full of NUL bytes in a regular pattern,
/// which legacy and UTF-8 text never has. The candidate must also decode without errors.
fn sniff_wide(bytes: &[u8]) -> Option<TextEncoding> {
    if bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    if bytes.len().is_multiple_of(4) {
        if bytes.as_chunks::<4>().0.iter().all(|unit| unit[3] == 0) && !decode_utf32(bytes, false).1
        {
            return Some(TextEncoding::Utf32Le);
        }
        if bytes.as_chunks::<4>().0.iter().all(|unit| unit[0] == 0) && !decode_utf32(bytes, true).1
        {
            return Some(TextEncoding::Utf32Be);
        }
    }
    let (mut even, mut odd) = (0usize, 0usize);
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == 0 {
            if index.is_multiple_of(2) {
                even += 1;
            } else {
                odd += 1;
            }
        }
    }
    let threshold = (bytes.len() / 8).max(1);
    if odd >= threshold && even * 8 <= odd && !decode_utf16(bytes, false).1 {
        return Some(TextEncoding::Utf16Le);
    }
    if even >= threshold && odd * 8 <= even && !decode_utf16(bytes, true).1 {
        return Some(TextEncoding::Utf16Be);
    }
    None
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> (String, bool) {
    let units = bytes.as_chunks::<2>().0.iter().map(|pair| {
        if big_endian {
            u16::from_be_bytes(*pair)
        } else {
            u16::from_le_bytes(*pair)
        }
    });
    let mut lossy = false;
    let mut text: String = char::decode_utf16(units)
        .map(|unit| {
            unit.unwrap_or_else(|_| {
                lossy = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    if !bytes.len().is_multiple_of(2) {
        text.push(char::REPLACEMENT_CHARACTER);
        lossy = true;
    }
    (text, lossy)
}

fn decode_utf32(bytes: &[u8], big_endian: bool) -> (String, bool) {
    let mut lossy = false;
    let mut text: String = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|quad| {
            let value = if big_endian {
                u32::from_be_bytes(*quad)
            } else {
                u32::from_le_bytes(*quad)
            };
            char::from_u32(value).unwrap_or_else(|| {
                lossy = true;
                char::REPLACEMENT_CHARACTER
            })
        })
        .collect();
    if !bytes.len().is_multiple_of(4) {
        text.push(char::REPLACEMENT_CHARACTER);
        lossy = true;
    }
    (text, lossy)
}

/// Converts CRLF and lone CR to `\n`; returns the majority style and whether styles were mixed.
fn normalize_line_endings(text: String) -> (String, LineEnding, bool) {
    let bytes = text.as_bytes();
    let (mut lf, mut crlf, mut cr) = (0usize, 0usize, 0usize);
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => {
                crlf += 1;
                index += 1;
            }
            b'\r' => cr += 1,
            b'\n' => lf += 1,
            _ => {}
        }
        index += 1;
    }
    let mixed = usize::from(lf > 0) + usize::from(crlf > 0) + usize::from(cr > 0) > 1;
    let majority = if lf >= crlf && lf >= cr {
        LineEnding::Lf
    } else if crlf >= cr {
        LineEnding::Crlf
    } else {
        LineEnding::Cr
    };
    if crlf == 0 && cr == 0 {
        return (text, majority, mixed);
    }
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(c);
        }
    }
    (normalized, majority, mixed)
}

fn unmappable(encoding: &'static Encoding, text: &str) -> EncodeError {
    let mut total = 0;
    let mut first = Vec::new();
    let (mut line, mut column) = (1, 1);
    let mut buffer = [0u8; 4];
    for (offset, ch) in text.char_indices() {
        let (_, _, had_errors) = encoding.encode(ch.encode_utf8(&mut buffer));
        if had_errors {
            total += 1;
            if first.len() < MAX_REPORTED_UNMAPPABLE {
                first.push(UnmappableChar {
                    offset,
                    line,
                    column,
                    ch,
                });
            }
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    EncodeError::Unmappable {
        encoding: encoding.name(),
        total,
        first,
    }
}

#[cfg(test)]
mod tests {
    use encoding_rs::SHIFT_JIS;

    use super::*;

    #[test]
    fn a_guess_is_kept_only_when_it_reproduces_the_bytes() {
        // 日本語: valid Shift_JIS that encodes back to itself.
        let exact = b"\x93\xFA\x96\x7B\x8C\xEA";
        assert_eq!(
            lossless_legacy(SHIFT_JIS, exact),
            TextEncoding::Legacy(SHIFT_JIS)
        );
        // A dangling lead byte cannot be decoded without replacement.
        let dangling = b"\x93\xFA\x93";
        assert_eq!(
            lossless_legacy(SHIFT_JIS, dangling),
            TextEncoding::Legacy(WINDOWS_1252)
        );
        // NEC row 13 holds a second code for U+2229: it decodes without errors but is written
        // back as the JIS X 0208 one.
        let duplicate = b"\x87\x9B";
        assert_eq!(
            SHIFT_JIS
                .decode_without_bom_handling_and_without_replacement(duplicate)
                .as_deref(),
            Some("\u{2229}")
        );
        assert_eq!(
            lossless_legacy(SHIFT_JIS, duplicate),
            TextEncoding::Legacy(WINDOWS_1252)
        );
        // GB18030 reads 0x80 as the euro sign but writes that as two bytes.
        assert_eq!(
            GB18030
                .decode_without_bom_handling_and_without_replacement(b"\x80")
                .as_deref(),
            Some("\u{20AC}")
        );
        assert_eq!(
            lossless_legacy(GB18030, b"\x80"),
            TextEncoding::Legacy(WINDOWS_1252)
        );
    }
}
