//! The Windows clipboard: Unicode text, `HTML Format`, `Rich Text Format`, PNG / DIB images,
//! `CF_HDROP` file lists and custom registered formats (such as `vscode-editor-data`).

use std::path::PathBuf;
use std::time::Duration;

use ssw_ui::services::{Clipboard, ClipboardContent, ClipboardFormat, ServiceError};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::core::HSTRING;

use super::failed;
use crate::wic::{DecodedImage, Wic, premultiply, unpremultiply};

const SERVICE: &str = "clipboard";

pub const FORMAT_HTML: &str = "HTML Format";
pub const FORMAT_RTF: &str = "Rich Text Format";
pub const FORMAT_PNG: &str = "PNG";
/// The format VS Code puts next to copied text; its JSON carries the source language mode.
pub const FORMAT_VSCODE_EDITOR_DATA: &str = "vscode-editor-data";

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_HDROP: u32 = 15;
const CF_LOCALE: u32 = 16;
const CF_DIBV5: u32 = 17;

/// First id of registered (named) clipboard formats.
const FIRST_REGISTERED: u32 = 0xC000;
const OPEN_ATTEMPTS: u32 = 10;
const OPEN_RETRY: Duration = Duration::from_millis(10);

/// Size of `DROPFILES`: offset, point, non-client flag, wide flag.
const DROPFILES_SIZE: usize = 20;
const CF_HTML_HEADER_LEN: usize = 105;

/// Normalizes line breaks to `\r\n` and encodes UTF-16LE with a NUL, as `CF_UNICODETEXT` wants.
#[must_use]
pub fn encode_text(text: &str) -> Vec<u8> {
    let crlf = text.replace("\r\n", "\n").replace('\n', "\r\n");
    crlf.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

/// Decodes `CF_UNICODETEXT` (up to the first NUL) and normalizes `\r\n` to `\n`.
#[must_use]
pub fn decode_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units).replace("\r\n", "\n")
}

/// Wraps an HTML fragment in a `CF_HTML` (`HTML Format`) payload with byte offsets in the
/// header.
#[must_use]
pub fn build_cf_html(fragment: &str) -> Vec<u8> {
    const PREFIX: &str = "<html>\r\n<body>\r\n<!--StartFragment-->";
    const SUFFIX: &str = "<!--EndFragment-->\r\n</body>\r\n</html>";
    let start_html = CF_HTML_HEADER_LEN;
    let start_fragment = start_html + PREFIX.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + SUFFIX.len();
    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n"
    );
    debug_assert_eq!(header.len(), CF_HTML_HEADER_LEN);
    let mut bytes = Vec::with_capacity(end_html + 1);
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(PREFIX.as_bytes());
    bytes.extend_from_slice(fragment.as_bytes());
    bytes.extend_from_slice(SUFFIX.as_bytes());
    bytes.push(0);
    bytes
}

fn header_offset(text: &str, key: &str) -> Option<usize> {
    let line = text.lines().find_map(|line| line.strip_prefix(key))?;
    line.trim().parse().ok()
}

/// Extracts the fragment of a `CF_HTML` payload: the `StartFragment` / `EndFragment` byte
/// range, or the text between the fragment comments when the offsets are missing or invalid.
#[must_use]
pub fn parse_cf_html(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    let text = String::from_utf8_lossy(bytes);
    let header = text.split("<html").next().unwrap_or("");
    let by_offsets = header_offset(header, "StartFragment:")
        .zip(header_offset(header, "EndFragment:"))
        .and_then(|(start, end)| bytes.get(start..end.min(bytes.len())))
        .filter(|slice| !slice.is_empty())
        .map(|slice| String::from_utf8_lossy(slice).into_owned());
    by_offsets.or_else(|| {
        let start = text.find("<!--StartFragment-->")? + "<!--StartFragment-->".len();
        let stop = text[start..].find("<!--EndFragment-->")? + start;
        Some(text[start..stop].to_owned())
    })
}

/// A `DROPFILES` block listing `paths` (wide characters, double-NUL terminated).
#[must_use]
pub fn build_hdrop(paths: &[PathBuf]) -> Vec<u8> {
    let mut bytes = vec![0u8; DROPFILES_SIZE];
    bytes[0..4].copy_from_slice(&(DROPFILES_SIZE as u32).to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    for path in paths {
        let wide: Vec<u16> = path.to_string_lossy().encode_utf16().collect();
        bytes.extend(wide.iter().flat_map(|unit| unit.to_le_bytes()));
        bytes.extend_from_slice(&[0, 0]);
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes
}

/// The paths of a `DROPFILES` block (wide or ANSI).
#[must_use]
pub fn parse_hdrop(bytes: &[u8]) -> Vec<PathBuf> {
    if bytes.len() < DROPFILES_SIZE {
        return Vec::new();
    }
    let offset = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let wide = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) != 0;
    let Some(list) = bytes.get(offset..) else {
        return Vec::new();
    };
    if wide {
        let units: Vec<u16> = list
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        units
            .split(|&unit| unit == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| PathBuf::from(String::from_utf16_lossy(name)))
            .collect()
    } else {
        list.split(|&byte| byte == 0)
            .take_while(|name| !name.is_empty())
            .map(|name| PathBuf::from(String::from_utf8_lossy(name).into_owned()))
            .collect()
    }
}

/// A top-down 32-bit `CF_DIB` (BITMAPINFOHEADER + straight-alpha BGRA) for `image`.
#[must_use]
pub fn build_dib(image: &DecodedImage) -> Vec<u8> {
    let pixels = unpremultiply(&image.bgra);
    let mut bytes = Vec::with_capacity(40 + pixels.len());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&(image.width as i32).to_le_bytes());
    bytes.extend_from_slice(&(-(image.height as i32)).to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&pixels);
    bytes
}

/// Wraps a packed DIB (header + pixels) in a BMP file so WIC can decode it.
#[must_use]
pub fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes([dib[0], dib[1], dib[2], dib[3]]) as usize;
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]) as usize;
    let compression = u32::from_le_bytes([dib[16], dib[17], dib[18], dib[19]]);
    let colors_used = u32::from_le_bytes([dib[32], dib[33], dib[34], dib[35]]) as usize;
    let masks = if compression == 3 && header_size == 40 {
        12
    } else {
        0
    };
    let palette_entries = if bit_count <= 8 {
        if colors_used > 0 {
            colors_used
        } else {
            1 << bit_count
        }
    } else {
        colors_used
    };
    let palette = palette_entries * 4;
    let offset = 14 + header_size + masks + palette;
    let total = 14 + dib.len();
    let mut bmp = Vec::with_capacity(total);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&(total as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
}

/// The `"mode"` string of `vscode-editor-data` JSON (for example `markdown`).
#[must_use]
pub fn parse_vscode_mode(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let after_key = &text[text.find("\"mode\"")? + "\"mode\"".len()..];
    let after_colon = after_key.trim_start().strip_prefix(':')?.trim_start();
    let value = after_colon.strip_prefix('"')?;
    let mut mode = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(mode),
            '\\' => mode.push(chars.next()?),
            other => mode.push(other),
        }
    }
    None
}

/// Closes the clipboard when dropped.
struct Open;

impl Open {
    fn acquire(owner: HWND) -> Result<Self, ServiceError> {
        for attempt in 0..OPEN_ATTEMPTS {
            // SAFETY: opens the clipboard for this thread; closed by `Drop`.
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Ok(Self);
            }
            if attempt + 1 < OPEN_ATTEMPTS {
                std::thread::sleep(OPEN_RETRY);
            }
        }
        Err(ServiceError::Unavailable { service: SERVICE })
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: balances the successful `OpenClipboard` of `acquire`.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn register(name: &str) -> u32 {
    // SAFETY: valid NUL-terminated name.
    unsafe { RegisterClipboardFormatW(&HSTRING::from(name)) }
}

fn format_name(format: u32) -> Option<String> {
    let mut buffer = [0u16; 128];
    // SAFETY: writable buffer of the stated length.
    let length = unsafe { GetClipboardFormatNameW(format, &mut buffer) };
    (length > 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
}

/// The system clipboard.
#[derive(Debug, Clone)]
pub struct WindowsClipboard {
    owner: HWND,
    wic: Wic,
}

impl WindowsClipboard {
    /// A clipboard owned by `owner` (a window that outlives the clipboard use).
    #[must_use]
    pub fn new(owner: HWND, wic: Wic) -> Self {
        Self { owner, wic }
    }

    fn put(format: u32, bytes: &[u8]) -> Result<(), ServiceError> {
        // SAFETY: the allocation is filled while locked; on success the clipboard owns the
        // handle, otherwise it is freed here.
        unsafe {
            let handle: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))
                .map_err(|error| failed(SERVICE, error))?;
            let pointer = GlobalLock(handle).cast::<u8>();
            if pointer.is_null() {
                let _ = GlobalFree(Some(handle));
                return Err(failed(SERVICE, "GlobalLock returned null"));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            let _ = GlobalUnlock(handle);
            if let Err(error) = SetClipboardData(format, Some(HANDLE(handle.0))) {
                let _ = GlobalFree(Some(handle));
                return Err(failed(SERVICE, error));
            }
        }
        Ok(())
    }

    fn fetch(format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open (callers hold `Open`); the handle is valid while the
        // clipboard stays open, and the copy stays within `GlobalSize`.
        unsafe {
            if IsClipboardFormatAvailable(format).is_err() {
                return None;
            }
            let handle = GetClipboardData(format).ok()?;
            let global = HGLOBAL(handle.0);
            let size = GlobalSize(global);
            let pointer = GlobalLock(global).cast::<u8>();
            if pointer.is_null() {
                return None;
            }
            let bytes = std::slice::from_raw_parts(pointer, size).to_vec();
            let _ = GlobalUnlock(global);
            Some(bytes)
        }
    }

    fn read<T>(&self, read: impl FnOnce() -> T) -> Result<T, ServiceError> {
        let _open = Open::acquire(self.owner)?;
        Ok(read())
    }

    /// The source language mode of copied VS Code text, if the clipboard carries
    /// `vscode-editor-data`.
    ///
    /// # Errors
    /// Fails when the clipboard cannot be read.
    pub fn vscode_mode(&self) -> Result<Option<String>, ServiceError> {
        Ok(self
            .custom(FORMAT_VSCODE_EDITOR_DATA)?
            .and_then(|bytes| parse_vscode_mode(&bytes)))
    }
}

impl Clipboard for WindowsClipboard {
    fn set(&mut self, content: ClipboardContent) -> Result<(), ServiceError> {
        let _open = Open::acquire(self.owner)?;
        // SAFETY: the clipboard is open.
        unsafe { EmptyClipboard() }.map_err(|error| failed(SERVICE, error))?;
        if let Some(text) = &content.text {
            Self::put(CF_UNICODETEXT, &encode_text(text))?;
        }
        if let Some(html) = &content.html {
            Self::put(register(FORMAT_HTML), &build_cf_html(html))?;
        }
        if let Some(rtf) = &content.rtf {
            let mut bytes = rtf.clone().into_bytes();
            bytes.push(0);
            Self::put(register(FORMAT_RTF), &bytes)?;
        }
        if let Some(png) = &content.image {
            Self::put(register(FORMAT_PNG), png)?;
            let decoded = self
                .wic
                .decode(png)
                .map_err(|error| failed(SERVICE, error))?;
            Self::put(CF_DIB, &build_dib(&decoded))?;
        }
        if !content.files.is_empty() {
            Self::put(CF_HDROP, &build_hdrop(&content.files))?;
        }
        for (name, bytes) in &content.custom {
            Self::put(register(name), bytes)?;
        }
        Ok(())
    }

    fn clear(&mut self) -> Result<(), ServiceError> {
        let _open = Open::acquire(self.owner)?;
        // SAFETY: the clipboard is open.
        unsafe { EmptyClipboard() }.map_err(|error| failed(SERVICE, error))
    }

    fn formats(&self) -> Result<Vec<ClipboardFormat>, ServiceError> {
        self.read(|| {
            let mut formats = Vec::new();
            let mut push = |format: ClipboardFormat| {
                if !formats.contains(&format) {
                    formats.push(format);
                }
            };
            let mut current = 0u32;
            loop {
                // SAFETY: the clipboard is open.
                current = unsafe { EnumClipboardFormats(current) };
                if current == 0 {
                    break;
                }
                match current {
                    CF_UNICODETEXT | CF_TEXT | CF_OEMTEXT => push(ClipboardFormat::Text),
                    CF_DIB | CF_DIBV5 | CF_BITMAP => push(ClipboardFormat::Image),
                    CF_HDROP => push(ClipboardFormat::Files),
                    CF_LOCALE => {}
                    id if id >= FIRST_REGISTERED => match format_name(id).as_deref() {
                        Some(FORMAT_HTML) => push(ClipboardFormat::Html),
                        Some(FORMAT_RTF) => push(ClipboardFormat::Rtf),
                        Some(FORMAT_PNG) => push(ClipboardFormat::Image),
                        Some(name) => push(ClipboardFormat::Custom(name.to_owned())),
                        None => {}
                    },
                    _ => {}
                }
            }
            formats
        })
    }

    fn text(&self) -> Result<Option<String>, ServiceError> {
        self.read(|| Self::fetch(CF_UNICODETEXT).map(|bytes| decode_text(&bytes)))
    }

    fn html(&self) -> Result<Option<String>, ServiceError> {
        self.read(|| Self::fetch(register(FORMAT_HTML)).and_then(|bytes| parse_cf_html(&bytes)))
    }

    fn rtf(&self) -> Result<Option<String>, ServiceError> {
        self.read(|| {
            Self::fetch(register(FORMAT_RTF)).map(|bytes| {
                let end = bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(bytes.len());
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            })
        })
    }

    fn image(&self) -> Result<Option<Vec<u8>>, ServiceError> {
        let fetched = self.read(|| {
            Self::fetch(register(FORMAT_PNG))
                .map(|png| (Some(png), None))
                .or_else(|| {
                    Self::fetch(CF_DIBV5)
                        .or_else(|| Self::fetch(CF_DIB))
                        .map(|dib| (None, Some(dib)))
                })
        })?;
        match fetched {
            None => Ok(None),
            Some((Some(png), _)) => Ok(Some(png)),
            Some((None, Some(dib))) => {
                let bmp = dib_to_bmp(&dib).ok_or_else(|| failed(SERVICE, "malformed DIB"))?;
                let decoded = self
                    .wic
                    .decode(&bmp)
                    .map_err(|error| failed(SERVICE, error))?;
                let straight = DecodedImage {
                    bgra: premultiply(&unpremultiply(&decoded.bgra)),
                    ..decoded
                };
                self.wic
                    .encode_png(&straight)
                    .map(Some)
                    .map_err(|error| failed(SERVICE, error))
            }
            Some((None, None)) => Ok(None),
        }
    }

    fn files(&self) -> Result<Vec<PathBuf>, ServiceError> {
        self.read(|| {
            Self::fetch(CF_HDROP)
                .map(|bytes| parse_hdrop(&bytes))
                .unwrap_or_default()
        })
    }

    fn custom(&self, name: &str) -> Result<Option<Vec<u8>>, ServiceError> {
        self.read(|| Self::fetch(register(name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_crlf_normalized_both_ways() {
        let bytes = encode_text("a\nb\r\nç😀");
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
        assert_eq!(decode_text(&bytes), "a\nb\nç😀");
        assert_eq!(
            decode_text(&[b'x', 0, 0, 0, b'y', 0]),
            "x",
            "stops at the NUL"
        );
        assert_eq!(decode_text(&[]), "");
        let crlf: Vec<u16> = "a\r\nb".encode_utf16().collect();
        let raw: Vec<u8> = crlf.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        assert_eq!(&encode_text("a\nb")[..raw.len()], &raw[..]);
    }

    #[test]
    fn cf_html_has_correct_offsets_and_round_trips() {
        let payload = build_cf_html("<b>héllo</b>");
        let text = String::from_utf8(payload[..payload.len() - 1].to_vec()).unwrap();
        assert!(text.starts_with("Version:0.9\r\nStartHTML:0000000105\r\n"));
        let number = |key: &str| -> usize {
            let at = text.find(key).unwrap() + key.len();
            text[at..at + 10].parse().unwrap()
        };
        assert_eq!(number("StartHTML:"), 105);
        assert_eq!(number("EndHTML:"), payload.len() - 1);
        assert_eq!(
            &payload[number("StartFragment:")..number("EndFragment:")],
            "<b>héllo</b>".as_bytes()
        );
        assert_eq!(parse_cf_html(&payload).as_deref(), Some("<b>héllo</b>"));
        assert_eq!(*payload.last().unwrap(), 0);
    }

    #[test]
    fn cf_html_parsing_falls_back_to_the_markers() {
        let no_offsets = b"Version:0.9\r\n<html><body><!--StartFragment--><i>x</i><!--EndFragment--></body></html>";
        assert_eq!(parse_cf_html(no_offsets).as_deref(), Some("<i>x</i>"));
        let bad_offsets = b"Version:0.9\r\nStartFragment:0000009999\r\nEndFragment:0000009999\r\n<html><!--StartFragment-->y<!--EndFragment--></html>";
        assert_eq!(parse_cf_html(bad_offsets).as_deref(), Some("y"));
        assert_eq!(parse_cf_html(b"plain text"), None);
        assert_eq!(parse_cf_html(b""), None);
    }

    #[test]
    fn hdrop_round_trips_wide_and_ansi() {
        let paths = vec![
            PathBuf::from(r"C:\Users\İsmail\a b.md"),
            PathBuf::from(r"D:\x\ç.txt"),
        ];
        let block = build_hdrop(&paths);
        assert_eq!(parse_hdrop(&block), paths);
        assert!(parse_hdrop(&block[..10]).is_empty());
        let mut ansi = vec![0u8; DROPFILES_SIZE];
        ansi[0] = DROPFILES_SIZE as u8;
        ansi.extend_from_slice(b"C:\\one.txt\0C:\\two.txt\0\0");
        assert_eq!(
            parse_hdrop(&ansi),
            vec![PathBuf::from("C:\\one.txt"), PathBuf::from("C:\\two.txt")]
        );
        let mut bad_offset = block.clone();
        bad_offset[0..4].copy_from_slice(&9999u32.to_le_bytes());
        assert!(parse_hdrop(&bad_offset).is_empty());
        assert_eq!(parse_hdrop(&build_hdrop(&[])), Vec::<PathBuf>::new());
    }

    #[test]
    fn dib_conversion_produces_a_decodable_bmp() {
        let image = DecodedImage {
            width: 2,
            height: 1,
            bgra: vec![0, 0, 255, 255, 255, 0, 0, 255],
        };
        let dib = build_dib(&image);
        assert_eq!(dib.len(), 40 + 8);
        assert_eq!(
            i32::from_le_bytes([dib[8], dib[9], dib[10], dib[11]]),
            -1,
            "top-down"
        );
        let bmp = dib_to_bmp(&dib).unwrap();
        assert_eq!(&bmp[..2], b"BM");
        assert_eq!(u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]), 54);
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let decoded = wic.decode(&bmp).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 1));
        assert_eq!(decoded.pixel(0, 0), Some([0, 0, 255, 255]));
        assert_eq!(dib_to_bmp(&[0; 10]), None);
        // Palette and bitfield sizes move the pixel data offset.
        let mut paletted = vec![0u8; 40];
        paletted[0] = 40;
        paletted[14] = 8;
        assert_eq!(
            u32::from_le_bytes(dib_to_bmp(&paletted).unwrap()[10..14].try_into().unwrap()),
            14 + 40 + 1024
        );
        let mut bitfields = vec![0u8; 40];
        bitfields[0] = 40;
        bitfields[14] = 32;
        bitfields[16] = 3;
        assert_eq!(
            u32::from_le_bytes(dib_to_bmp(&bitfields).unwrap()[10..14].try_into().unwrap()),
            14 + 40 + 12
        );
    }

    #[test]
    fn vscode_mode_is_extracted_from_json() {
        let json = br#"{"version":1,"isFromEmptySelection":false,"multicursorText":null,"mode":"markdown"}"#;
        assert_eq!(parse_vscode_mode(json).as_deref(), Some("markdown"));
        assert_eq!(
            parse_vscode_mode(br#"{ "mode" : "type\"script" }"#).as_deref(),
            Some("type\"script")
        );
        assert_eq!(parse_vscode_mode(br#"{"mode":null}"#), None);
        assert_eq!(parse_vscode_mode(br#"{"mode":"unterminated"#), None);
        assert_eq!(parse_vscode_mode(b"{}"), None);
        assert_eq!(parse_vscode_mode(br#"{"mode" "x"}"#), None);
        assert_eq!(parse_vscode_mode(br#"{"mode":"a\"#), None);
    }
}
