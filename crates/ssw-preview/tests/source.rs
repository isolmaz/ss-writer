//! Reading Explorer's stream and decoding it: size cap, failing and size-less streams, byte
//! order marks, BOM-less UTF-16, the ANSI fallback, line endings and the stream's file name.

mod common;

use common::{Fault, memory_stream, scripted_stream};
use ssw_preview::source::{
    MAX_PREVIEW_BYTES, SourceError, decode_ansi, decode_text, is_mermaid_name, normalize_newlines,
    read_stream, stream_name,
};
use windows::Win32::System::Com::{IStream, STGM_READ, STREAM_SEEK_SET};
use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
use windows::core::HSTRING;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[test]
fn the_default_limit_is_four_mebibytes() {
    assert_eq!(MAX_PREVIEW_BYTES, 4 * 1024 * 1024);
}

#[test]
fn a_stream_is_read_whole_from_its_start_over_several_chunks() {
    let bytes = pattern(150_000);
    let stream = memory_stream(&bytes);
    // SAFETY: plain call on a live stream; the position out pointer is optional.
    unsafe { stream.Seek(5, STREAM_SEEK_SET, None) }.unwrap();
    assert_eq!(read_stream(&stream, 1_000_000).unwrap(), bytes);
    assert!(read_stream(&memory_stream(b""), 10).unwrap().is_empty());
}

#[test]
fn a_known_size_over_the_limit_is_refused_and_the_limit_itself_is_fine() {
    let stream = memory_stream(&pattern(100));
    assert_eq!(read_stream(&stream, 100).unwrap().len(), 100);
    let error = read_stream(&stream, 99).unwrap_err();
    assert!(
        matches!(error, SourceError::TooLarge { limit: 99 }),
        "{error:?}"
    );
    assert!(error.to_string().contains("99"));
}

#[test]
fn a_stream_without_a_size_is_cut_off_at_the_limit() {
    let data = pattern(200_000);
    let ok = scripted_stream(Fault::NoSize, &data);
    assert_eq!(read_stream(&ok, 1_000_000).unwrap(), data);
    let over = scripted_stream(Fault::NoSize, &data);
    assert!(matches!(
        read_stream(&over, 199_999),
        Err(SourceError::TooLarge { limit: 199_999 })
    ));
    let endless = scripted_stream(Fault::Endless, &[]);
    assert!(matches!(
        read_stream(&endless, 1_000),
        Err(SourceError::TooLarge { limit: 1_000 })
    ));
}

#[test]
fn a_failing_read_is_reported() {
    let stream = scripted_stream(Fault::FailRead, &[]);
    let error = read_stream(&stream, 1_000).unwrap_err();
    assert!(matches!(error, SourceError::Read(_)), "{error:?}");
    assert!(error.to_string().starts_with("reading the file failed"));
}

#[test]
fn byte_order_marks_choose_the_encoding_and_are_dropped() {
    let mut utf8 = vec![0xEF, 0xBB, 0xBF];
    utf8.extend_from_slice("héllo\r\n".as_bytes());
    assert_eq!(decode_text(&utf8), "héllo\n");
    let mut le = vec![0xFF, 0xFE];
    le.extend("hi\r\nyo 😀".encode_utf16().flat_map(u16::to_le_bytes));
    assert_eq!(decode_text(&le), "hi\nyo 😀");
    let mut be = vec![0xFE, 0xFF];
    be.extend("hi\ryo".encode_utf16().flat_map(u16::to_be_bytes));
    assert_eq!(decode_text(&be), "hi\nyo");
}

#[test]
fn utf16_without_a_mark_is_recognized_by_its_null_bytes() {
    let le: Vec<u8> = "hello world"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    assert_eq!(decode_text(&le), "hello world");
    let be: Vec<u8> = "hello world"
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    assert_eq!(decode_text(&be), "hello world");
    assert_eq!(decode_text(b"a\0b\0c\0"), "abc");
    // A stray NUL in UTF-8 text is not UTF-16.
    assert_eq!(decode_text(b"hello\0world"), "hello\0world");
}

#[test]
fn utf16_damage_becomes_replacement_characters_and_odd_bytes_are_dropped() {
    assert_eq!(decode_text(&[0xFF, 0xFE, 0x41, 0x00, 0x42]), "A");
    assert_eq!(decode_text(&[0xFF, 0xFE, 0x00, 0xD8]), "\u{FFFD}");
}

#[test]
fn utf8_without_a_mark_and_text_in_the_ansi_code_page() {
    assert_eq!(decode_text("日本語\r\nx".as_bytes()), "日本語\nx");
    assert_eq!(decode_text(b""), "");
    // Not UTF-8: the system code page decides the rest, but the text is never lost.
    let decoded = decode_text(b"caf\xe9");
    assert!(
        decoded.starts_with("caf") && decoded.chars().count() >= 4,
        "{decoded:?}"
    );
    assert_eq!(decode_ansi(b"caf\xe9", 1252), "café");
    assert_eq!(decode_ansi(&[0x80], 1252), "€");
    assert_eq!(decode_ansi(b"", 1252), "");
    // An unknown code page falls back to UTF-8 with replacement characters.
    assert_eq!(decode_ansi(b"ab\xff", 99_999), "ab\u{FFFD}");
}

#[test]
fn line_endings_become_newlines() {
    assert_eq!(normalize_newlines("a\r\nb\rc\n\r\nd\r"), "a\nb\nc\n\nd\n");
    assert_eq!(normalize_newlines("plain\n"), "plain\n");
}

#[test]
fn only_mmd_names_are_diagrams() {
    assert!(is_mermaid_name("a.mmd"));
    assert!(is_mermaid_name(r"C:\docs\FLOW.MMD"));
    assert!(!is_mermaid_name("a.md"));
    assert!(!is_mermaid_name("a.mmdx"));
    assert!(!is_mermaid_name("mmd"));
    assert!(!is_mermaid_name("dir.mmd/readme"));
}

#[test]
fn the_stream_name_is_read_when_the_stream_has_one() {
    assert_eq!(
        stream_name(&memory_stream(b"x")),
        None,
        "memory streams are anonymous"
    );
    assert_eq!(
        stream_name(&scripted_stream(Fault::NoSize, &[])),
        None,
        "Stat fails"
    );
    let path = std::env::temp_dir().join("ssw_preview_stream_name_test.mmd");
    std::fs::write(&path, "graph TD\n").unwrap();
    // SAFETY: a valid path string and no template stream.
    let stream: IStream = unsafe {
        SHCreateStreamOnFileEx(
            &HSTRING::from(path.as_os_str()),
            STGM_READ.0,
            0,
            false,
            None::<&IStream>,
        )
    }
    .unwrap();
    let name = stream_name(&stream);
    drop(stream);
    std::fs::remove_file(&path).unwrap();
    if let Some(name) = name {
        assert!(is_mermaid_name(&name), "{name}");
    }
}
