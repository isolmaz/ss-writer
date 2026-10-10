//! The bytes of the previewed file: reading Explorer's `IStream` with a size cap and decoding
//! them to the text the document holds.
//!
//! The handler has no path (`IInitializeWithStream` only), so there is no encoding detection
//! beyond what the bytes themselves say: BOMs, UTF-16 without BOM (a null byte in every other
//! position), valid UTF-8, and otherwise the system ANSI code page.

use thiserror::Error;
use windows::Win32::Globalization::{CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS, MultiByteToWideChar};
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};

/// The largest file the preview renders, in bytes. Bigger streams show a notice instead.
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024 * 1024;

/// Bytes requested per `IStream::Read` call.
const CHUNK: usize = 64 * 1024;

/// How many bytes are examined to recognize UTF-16 without a byte order mark.
const SNIFF_BYTES: usize = 4096;

/// Why the stream could not be turned into bytes.
#[derive(Debug, Error)]
pub enum SourceError {
    /// The stream holds more than the preview limit.
    #[error("the file is larger than the {limit} byte preview limit")]
    TooLarge {
        /// The limit that was exceeded.
        limit: usize,
    },
    /// `IStream::Read` failed.
    #[error("reading the file failed: {0}")]
    Read(#[source] windows::core::Error),
}

/// Reads the whole stream, from its start, up to `limit` bytes.
///
/// A stream that reports a size above `limit` is rejected without reading it; one that cannot
/// report its size is read until it ends or exceeds `limit`.
///
/// # Errors
/// [`SourceError::TooLarge`] when the stream holds more than `limit` bytes,
/// [`SourceError::Read`] when the stream fails.
pub fn read_stream(stream: &IStream, limit: usize) -> Result<Vec<u8>, SourceError> {
    // The shell hands over streams at position 0; rewinding is a courtesy that may be refused.
    // SAFETY: plain call on a live stream; no out pointer is passed.
    let _ = unsafe { stream.Seek(0, STREAM_SEEK_SET, None) };
    let mut stat = STATSTG::default();
    // SAFETY: `stat` is a valid out pointer; `STATFLAG_NONAME` means no name is allocated that
    // would have to be freed.
    let known = unsafe { stream.Stat(&mut stat, STATFLAG_NONAME) }.is_ok();
    let mut bytes = Vec::new();
    if known {
        if stat.cbSize > limit as u64 {
            return Err(SourceError::TooLarge { limit });
        }
        // `cbSize <= limit`, which fits `usize`.
        bytes.reserve_exact(stat.cbSize as usize);
    }
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let mut read = 0u32;
        // SAFETY: `chunk` is valid for `CHUNK` bytes (which fits `u32`) and `read` is a valid
        // out pointer.
        let status =
            unsafe { stream.Read(chunk.as_mut_ptr().cast(), CHUNK as u32, Some(&raw mut read)) };
        if status.is_err() {
            return Err(SourceError::Read(status.into()));
        }
        if read == 0 {
            return Ok(bytes);
        }
        if bytes.len() + read as usize > limit {
            return Err(SourceError::TooLarge { limit });
        }
        bytes.extend_from_slice(&chunk[..read as usize]);
    }
}

/// The text of `bytes` with `\n` line endings.
///
/// A UTF-8 or UTF-16 byte order mark selects the encoding and is dropped; without one, UTF-16
/// is recognized by its null bytes, then UTF-8, and anything else is read in the system ANSI
/// code page. Invalid sequences become U+FFFD, never an error: a damaged file still previews.
#[must_use]
pub fn decode_text(bytes: &[u8]) -> String {
    let text = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(rest).into_owned()
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(rest, u16::from_be_bytes)
    } else if let Some(order) = sniff_utf16(bytes) {
        utf16(bytes, order)
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        text.to_owned()
    } else {
        ansi(bytes)
    };
    normalize_newlines(&text)
}

/// UTF-16 code units from byte pairs, `order` turning a pair into a unit. A trailing odd byte
/// is dropped.
fn utf16(bytes: &[u8], order: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| order(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

/// Recognizes UTF-16 without a byte order mark: Latin text has a zero in every other byte, and
/// never in the other position.
fn sniff_utf16(bytes: &[u8]) -> Option<fn([u8; 2]) -> u16> {
    let sample = &bytes[..bytes.len().min(SNIFF_BYTES) & !1];
    let pairs = sample.len() / 2;
    if pairs == 0 {
        return None;
    }
    let zeros = |offset: usize| {
        sample
            .as_chunks::<2>()
            .0
            .iter()
            .filter(|pair| pair[offset] == 0)
            .count()
    };
    let (even, odd) = (zeros(0), zeros(1));
    if even == 0 && odd * 10 >= pairs * 3 {
        Some(u16::from_le_bytes)
    } else if odd == 0 && even * 10 >= pairs * 3 {
        Some(u16::from_be_bytes)
    } else {
        None
    }
}

/// `bytes` read in the system ANSI code page.
fn ansi(bytes: &[u8]) -> String {
    decode_ansi(bytes, CP_ACP)
}

/// `bytes` read in Windows code page `code_page`. A code page the system does not know (or
/// empty input) falls back to UTF-8 with U+FFFD for invalid sequences.
#[must_use]
pub fn decode_ansi(bytes: &[u8], code_page: u32) -> String {
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    // SAFETY: a valid input slice; without an output buffer the call only measures.
    let needed = unsafe { MultiByteToWideChar(code_page, flags, bytes, None) };
    let mut units = vec![0u16; usize::try_from(needed).unwrap_or(0)];
    if units.is_empty() {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    // SAFETY: `units` holds exactly the number of units the measuring call asked for.
    let written =
        unsafe { MultiByteToWideChar(code_page, flags, bytes, Some(units.as_mut_slice())) };
    units.truncate(usize::try_from(written).unwrap_or(0));
    String::from_utf16_lossy(&units)
}

/// Replaces `\r\n` and lone `\r` with `\n` (the document stores `\n` only).
#[must_use]
pub fn normalize_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// The name the stream reports (the shell's file streams report the file name or path), if any.
#[must_use]
pub fn stream_name(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    // SAFETY: `stat` is a valid out pointer; with `STATFLAG_DEFAULT` the stream may allocate
    // the name with `CoTaskMemAlloc`, which is freed below.
    unsafe { stream.Stat(&mut stat, STATFLAG_DEFAULT) }.ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    // SAFETY: a non-null name returned by `Stat` is a NUL-terminated wide string that this
    // caller owns.
    let name = unsafe { stat.pwcsName.to_string() }.ok();
    // SAFETY: the name was allocated by the stream with the COM task allocator and is not used
    // after this call.
    unsafe { CoTaskMemFree(Some(stat.pwcsName.as_ptr().cast_const().cast())) };
    name
}

/// True for the name of a Mermaid diagram file: the whole file is one diagram, not Markdown.
#[must_use]
pub fn is_mermaid_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("mmd"))
}
