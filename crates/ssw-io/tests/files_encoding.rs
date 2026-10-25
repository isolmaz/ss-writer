use std::collections::HashSet;
use std::path::PathBuf;

use ssw_io::encoding::{
    EncodeError, LineEnding, MAX_REPORTED_UNMAPPABLE, TextEncoding, TextFormat, UnmappableChar,
    decode, decode_as, detect, encode, looks_binary, system_locale_name, system_tld_hint,
    tld_hint_for_locale,
};

fn fixture(name: &str) -> Vec<u8> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "tests",
        "fixtures",
        "text",
        name,
    ]
    .iter()
    .collect();
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

const FIXTURES: [&str; 16] = [
    "derived-ja-gb18030.txt",
    "derived-ja-shift_jis.txt",
    "derived-ja-utf-16be-bom.txt",
    "derived-ja-utf-16le-bom.txt",
    "derived-tr-utf-16be-bom.txt",
    "derived-tr-utf-16le-bom.txt",
    "derived-tr-utf-8-bom.txt",
    "derived-tr-windows-1254.txt",
    "uchardet-ja-shift_jis.txt",
    "uchardet-ja-utf-16be.txt",
    "uchardet-ja-utf-16le.txt",
    "uchardet-ja-utf-8.txt",
    "uchardet-tr-iso-8859-9.txt",
    "uchardet-tr-utf-8.txt",
    "uchardet-zh-gb18030.txt",
    "uchardet-zh-utf-8.txt",
];

#[test]
fn fixtures_round_trip_byte_identical() {
    for name in FIXTURES {
        let bytes = fixture(name);
        let decoded = decode(&bytes, Some("tr"));
        assert!(!decoded.lossy, "{name} decoded lossy");
        assert!(!decoded.text.contains('\r'), "{name} kept a CR");
        assert_eq!(
            encode(&decoded.text, &decoded.format).unwrap(),
            bytes,
            "{name}"
        );
        // Without a hint the file must still round-trip exactly.
        let plain = decode(&bytes, None);
        assert_eq!(
            encode(&plain.text, &plain.format).unwrap(),
            bytes,
            "{name} (no hint)"
        );
    }
}

#[test]
fn fixtures_detect_expected_encodings() {
    let cases: [(&str, TextEncoding, bool); 12] = [
        ("derived-tr-utf-8-bom.txt", TextEncoding::Utf8, true),
        ("derived-tr-utf-16le-bom.txt", TextEncoding::Utf16Le, true),
        ("derived-tr-utf-16be-bom.txt", TextEncoding::Utf16Be, true),
        ("derived-ja-utf-16le-bom.txt", TextEncoding::Utf16Le, true),
        ("derived-ja-utf-16be-bom.txt", TextEncoding::Utf16Be, true),
        ("uchardet-ja-utf-16le.txt", TextEncoding::Utf16Le, false),
        ("uchardet-ja-utf-16be.txt", TextEncoding::Utf16Be, false),
        ("uchardet-tr-utf-8.txt", TextEncoding::Utf8, false),
        ("uchardet-ja-utf-8.txt", TextEncoding::Utf8, false),
        ("uchardet-zh-utf-8.txt", TextEncoding::Utf8, false),
        (
            "derived-tr-windows-1254.txt",
            TextEncoding::from_label("windows-1254").unwrap(),
            false,
        ),
        (
            "uchardet-tr-iso-8859-9.txt",
            TextEncoding::from_label("windows-1254").unwrap(),
            false,
        ),
    ];
    for (name, encoding, bom) in cases {
        let decoded = decode(&fixture(name), Some("tr"));
        assert_eq!(decoded.format.encoding, encoding, "{name}");
        assert_eq!(decoded.format.bom, bom, "{name}");
    }
}

#[test]
fn turkish_text_decodes_with_dotless_i() {
    let decoded = decode(&fixture("derived-tr-windows-1254.txt"), Some("tr"));
    assert!(decoded.text.starts_with("Türkiye, resmî adıyla Türkiye"));
    assert_eq!(decoded.format.line_ending, LineEnding::Lf);
}

#[test]
fn bom_is_stripped_and_remembered() {
    let decoded = decode(b"\xEF\xBB\xBFhello\n", None);
    assert_eq!(decoded.text, "hello\n");
    assert!(decoded.format.bom);
    let utf8 = decode_as(b"\xEF\xBB\xBFhi", TextEncoding::Utf8);
    assert!(utf8.format.bom);
    // A legacy code page never strips a BOM-looking prefix.
    let legacy = decode_as(
        b"\xEF\xBB\xBFhi",
        TextEncoding::from_label("windows-1252").unwrap(),
    );
    assert!(!legacy.format.bom);
    assert_eq!(legacy.text, "ï»¿hi");
    // A BOM of another encoding is not stripped.
    assert!(!decode_as(b"\xFF\xFEa\x00", TextEncoding::Utf8).format.bom);
}

#[test]
fn utf32_with_and_without_bom() {
    let text = "héllo\nwörld ☃ 𝄞\n";
    for (encoding, bom) in [
        (TextEncoding::Utf32Le, true),
        (TextEncoding::Utf32Be, true),
        (TextEncoding::Utf32Le, false),
        (TextEncoding::Utf32Be, false),
        (TextEncoding::Utf16Le, false),
        (TextEncoding::Utf16Be, false),
    ] {
        let format = TextFormat {
            encoding,
            bom,
            line_ending: LineEnding::Lf,
            had_mixed_endings: false,
        };
        let bytes = encode(text, &format).unwrap();
        let decoded = decode(&bytes, None);
        assert_eq!(decoded.text, text, "{encoding:?} bom={bom}");
        assert_eq!(decoded.format.encoding, encoding);
        assert_eq!(decoded.format.bom, bom);
        assert!(!decoded.lossy);
    }
}

#[test]
fn tiny_wide_files_are_sniffed() {
    assert_eq!(detect(b"A\x00", None), TextEncoding::Utf16Le);
    assert_eq!(detect(b"\x00A", None), TextEncoding::Utf16Be);
    // Odd length can never be UTF-16, and an empty file is plain UTF-8.
    assert_eq!(detect(b"A\x00B", None), TextEncoding::Utf8);
    assert_eq!(detect(b"", None), TextEncoding::Utf8);
    assert_eq!(decode(b"", None).text, "");
}

#[test]
fn text_without_nul_bytes_is_not_binary() {
    assert!(!looks_binary(b"fn main() {}\n"));
    assert!(!looks_binary("héllo ☃\n".as_bytes()));
    // Legacy code pages and malformed UTF-8 are still text.
    assert!(!looks_binary(b"caf\xE9\n"));
    assert!(!looks_binary(b"\xC3\x28"));
    assert!(!looks_binary(b"\xEF\xBB\xBFwith a UTF-8 BOM\n"));
    // An empty file has nothing that says otherwise.
    assert!(!looks_binary(b""));
}

#[test]
fn a_nul_byte_makes_a_file_binary() {
    // The signature of a PNG and the header of an executable carry NUL bytes early.
    assert!(looks_binary(b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"));
    assert!(looks_binary(b"MZ\x90\x00\x03\x00\x00\x00"));
    assert!(looks_binary(b"\x00"));
    assert!(looks_binary(b"text and then\x00more text"));
    // A zero-filled start, as a disk image has, reads as UTF-32 whose characters are all NUL:
    // that is no text.
    assert!(looks_binary(&[0; 4]));
    assert!(looks_binary(&vec![0; 8192]));
    assert!(looks_binary(&vec![0; 1 << 20]));
    // Bytes that only start like UTF-16 or UTF-32 are still binary: the sniff finds no wide
    // text in them.
    assert!(looks_binary(b"A\x00B\x00\x00\x00"));
}

#[test]
fn utf16_and_utf32_text_without_a_bom_is_text_as_detect_finds_it() {
    // The same bytes open as text whatever the file is called: `detect` sniffs them, so the
    // binary check agrees with it.
    assert!(!looks_binary(b"A\x00B\x00"));
    assert!(!looks_binary(b"\x00A\x00B"));
    assert!(!looks_binary(b"A\x00\x00\x00"));
    assert!(!looks_binary(b"\x00\x00\x00A"));
    for (name, encoding) in [
        ("uchardet-ja-utf-16le.txt", TextEncoding::Utf16Le),
        ("uchardet-ja-utf-16be.txt", TextEncoding::Utf16Be),
    ] {
        let bytes = fixture(name);
        assert!(bytes.contains(&0), "{name} has no NUL byte");
        assert_eq!(detect(&bytes, None), encoding, "{name}");
        assert!(!looks_binary(&bytes), "{name}");
    }
}

#[test]
fn a_window_that_ends_inside_a_surrogate_pair_is_still_utf16_text() {
    // The 8192 bytes inspected end between the two halves of U+1F600.
    let mut text = "a".repeat(4095);
    text.push('\u{1F600}');
    text.push('b');
    for big_endian in [false, true] {
        let bytes: Vec<u8> = text
            .encode_utf16()
            .flat_map(|unit| {
                if big_endian {
                    unit.to_be_bytes()
                } else {
                    unit.to_le_bytes()
                }
            })
            .collect();
        assert_eq!(bytes.len(), 8196);
        assert!(!looks_binary(&bytes), "big endian: {big_endian}");
        // A caller that fetched only the window sees the same bytes.
        assert!(!looks_binary(&bytes[..8192]), "big endian: {big_endian}");
    }
}

#[test]
fn utf_text_is_text_with_or_without_a_bom_although_it_holds_nul_bytes() {
    let text = "héllo\nwörld ☃\n";
    for encoding in [
        TextEncoding::Utf8,
        TextEncoding::Utf16Le,
        TextEncoding::Utf16Be,
        TextEncoding::Utf32Le,
        TextEncoding::Utf32Be,
    ] {
        let mut format = TextFormat {
            encoding,
            bom: true,
            line_ending: LineEnding::Lf,
            had_mixed_endings: false,
        };
        let with_bom = encode(text, &format).unwrap();
        assert!(!looks_binary(&with_bom), "{encoding:?} with a BOM");
        // The same text without the BOM is text too, as `detect` finds its encoding.
        format.bom = false;
        let without_bom = encode(text, &format).unwrap();
        assert_eq!(detect(&without_bom, None), encoding, "{encoding:?}");
        assert!(!looks_binary(&without_bom), "{encoding:?} without a BOM");
    }
    // The BOM fixtures of the decoder tests are text too.
    for name in [
        "derived-tr-utf-16le-bom.txt",
        "derived-tr-utf-16be-bom.txt",
        "derived-ja-utf-16le-bom.txt",
    ] {
        let bytes = fixture(name);
        assert!(bytes.contains(&0), "{name} has no NUL byte");
        assert!(!looks_binary(&bytes), "{name}");
    }
}

#[test]
fn only_the_first_8192_bytes_are_inspected() {
    let mut bytes = vec![b'a'; 8192];
    bytes.push(0);
    assert!(!looks_binary(&bytes), "a NUL after 8192 bytes");
    bytes[8191] = 0;
    assert!(looks_binary(&bytes), "a NUL as the 8192nd byte");
    let mut late = vec![b'a'; 1 << 20];
    late[1 << 19] = 0;
    assert!(!looks_binary(&late), "a NUL half a megabyte in");
    // A BOM wins over a NUL inside the window.
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend_from_slice(b"a\x00b");
    assert!(!looks_binary(&bom));
}

#[test]
fn malformed_input_is_flagged_lossy() {
    // BOM says UTF-8 but the payload is not.
    let bad_utf8 = decode(b"\xEF\xBB\xBFab\xFFcd", None);
    assert!(bad_utf8.lossy);
    assert_eq!(bad_utf8.text, "ab\u{FFFD}cd");
    // Unpaired surrogate and a dangling byte in UTF-16.
    let bad_utf16 = decode(b"\xFF\xFE\x00\xD8", None);
    assert!(bad_utf16.lossy);
    assert_eq!(bad_utf16.text, "\u{FFFD}");
    let odd_utf16 = decode(b"\xFF\xFEa\x00b", None);
    assert!(odd_utf16.lossy);
    assert_eq!(odd_utf16.text, "a\u{FFFD}");
    // Code point above U+10FFFF and a dangling byte in UTF-32.
    let bad_utf32 = decode(b"\xFF\xFE\x00\x00\x00\x00\x11\x00", None);
    assert!(bad_utf32.lossy);
    assert_eq!(bad_utf32.text, "\u{FFFD}");
    let odd_utf32 = decode(b"\x00\x00\xFE\xFF\x00\x00\x00a\x00", None);
    assert!(odd_utf32.lossy);
    assert_eq!(odd_utf32.text, "a\u{FFFD}");
    // Forced legacy decoding with undefined bytes.
    let greek = decode_as(b"\xAA", TextEncoding::from_label("windows-1253").unwrap());
    assert!(greek.lossy);
}

#[test]
fn undecodable_guesses_fall_back_to_lossless_windows_1252() {
    // Shift-JIS text with a dangling lead byte cannot be reproduced by any CJK guess.
    let mut bytes = fixture("uchardet-ja-shift_jis.txt");
    bytes.push(0x93);
    let decoded = decode(&bytes, None);
    assert!(!decoded.lossy);
    assert_eq!(encode(&decoded.text, &decoded.format).unwrap(), bytes);
    // Bytes that are no valid text in any encoding still survive a round trip.
    let raw: Vec<u8> = (0x80u8..=0xFF).collect();
    let decoded = decode(&raw, None);
    assert_eq!(encode(&decoded.text, &decoded.format).unwrap(), raw);
}

#[test]
fn line_endings_use_the_majority_and_are_restored() {
    let cases: [(&[u8], LineEnding, bool, &str); 7] = [
        (b"a\nb\nc", LineEnding::Lf, false, "a\nb\nc"),
        (b"a\r\nb\r\nc", LineEnding::Crlf, false, "a\nb\nc"),
        (b"a\rb\rc", LineEnding::Cr, false, "a\nb\nc"),
        (b"a\r\nb\r\nc\nd", LineEnding::Crlf, true, "a\nb\nc\nd"),
        (b"a\r\nb\nc", LineEnding::Lf, true, "a\nb\nc"),
        (b"a\rb\rc\n", LineEnding::Cr, true, "a\nb\nc\n"),
        (b"no newline", LineEnding::Lf, false, "no newline"),
    ];
    for (bytes, ending, mixed, text) in cases {
        let decoded = decode(bytes, None);
        assert_eq!(decoded.text, text);
        assert_eq!(decoded.format.line_ending, ending, "{bytes:?}");
        assert_eq!(decoded.format.had_mixed_endings, mixed, "{bytes:?}");
        let again = encode(&decoded.text, &decoded.format).unwrap();
        assert_eq!(again, text.replace('\n', ending.as_str()).as_bytes());
    }
    assert_eq!(LineEnding::Crlf.label(), "CRLF");
    assert_eq!(LineEnding::Lf.label(), "LF");
    assert_eq!(LineEnding::Cr.label(), "CR");
    assert_eq!(LineEnding::default(), LineEnding::Lf);
}

#[test]
fn unmappable_characters_are_listed_with_positions() {
    let format = TextFormat {
        encoding: TextEncoding::from_label("windows-1254").unwrap(),
        ..TextFormat::default()
    };
    let error = encode("ok\nabc 日 x 本", &format).unwrap_err();
    assert_eq!(
        error,
        EncodeError::Unmappable {
            encoding: "windows-1254",
            total: 2,
            first: vec![
                UnmappableChar {
                    offset: 7,
                    line: 2,
                    column: 5,
                    ch: '日'
                },
                UnmappableChar {
                    offset: 13,
                    line: 2,
                    column: 9,
                    ch: '本'
                },
            ],
        }
    );
    assert_eq!(
        error.to_string(),
        "2 character(s) cannot be represented in windows-1254"
    );
    let many = encode(&"日".repeat(15), &format).unwrap_err();
    let EncodeError::Unmappable { total, first, .. } = many;
    assert_eq!((total, first.len()), (15, MAX_REPORTED_UNMAPPABLE));
    // Legacy encodings ignore the BOM flag.
    let with_bom = TextFormat {
        bom: true,
        ..format
    };
    assert_eq!(encode("abc", &with_bom).unwrap(), b"abc");
}

#[test]
fn encoding_names_labels_and_serde() {
    let names = [
        (TextEncoding::Utf8, "UTF-8"),
        (TextEncoding::Utf16Le, "UTF-16LE"),
        (TextEncoding::Utf16Be, "UTF-16BE"),
        (TextEncoding::Utf32Le, "UTF-32LE"),
        (TextEncoding::Utf32Be, "UTF-32BE"),
    ];
    for (encoding, name) in names {
        assert_eq!(encoding.name(), name);
        assert_eq!(TextEncoding::from_label(name), Some(encoding));
        assert!(encoding.supports_bom());
    }
    assert!(
        !TextEncoding::from_label("windows-1252")
            .unwrap()
            .supports_bom()
    );
    let labels = [
        ("utf8", Some(TextEncoding::Utf8)),
        (" UTF-8 ", Some(TextEncoding::Utf8)),
        ("utf-16", Some(TextEncoding::Utf16Le)),
        ("utf-32", Some(TextEncoding::Utf32Le)),
        ("unicode-1-1-utf-8", Some(TextEncoding::Utf8)),
        ("unicode", Some(TextEncoding::Utf16Le)),
        ("unicodefffe", Some(TextEncoding::Utf16Be)),
        ("iso-2022-kr", None),
        ("no-such-encoding", None),
    ];
    for (label, expected) in labels {
        assert_eq!(TextEncoding::from_label(label), expected, "{label}");
    }
    let format = TextFormat {
        encoding: TextEncoding::from_label("shift_jis").unwrap(),
        bom: false,
        line_ending: LineEnding::Crlf,
        had_mixed_endings: true,
    };
    let json = serde_json::to_string(&format).unwrap();
    assert!(json.contains("\"Shift_JIS\""));
    assert_eq!(serde_json::from_str::<TextFormat>(&json).unwrap(), format);
    let error =
        serde_json::from_str::<TextFormat>(&json.replace("Shift_JIS", "bogus")).unwrap_err();
    assert!(error.to_string().contains("unknown encoding"));
    assert_eq!(TextFormat::default().encoding, TextEncoding::Utf8);
    let selectable = TextEncoding::selectable();
    assert!(selectable.len() > 20);
    for encoding in selectable {
        assert_eq!(TextEncoding::from_label(encoding.name()), Some(encoding));
    }
}

#[test]
fn every_selectable_encoding_is_offered_once() {
    let names: Vec<&str> = TextEncoding::selectable()
        .into_iter()
        .map(TextEncoding::name)
        .collect();
    let distinct: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(distinct.len(), names.len(), "{names:?}");
    // ISO-8859-9 is the Turkish code page the menu offers as windows-1254.
    assert!(names.contains(&"windows-1254"), "{names:?}");
    assert_eq!(
        TextEncoding::from_label("iso-8859-9"),
        TextEncoding::from_label("windows-1254")
    );
}

#[test]
fn locale_names_map_to_tld_hints() {
    let cases = [
        ("tr-TR", Some("tr")),
        ("tr", Some("tr")),
        ("zh-Hans-CN", Some("cn")),
        ("en-GB", Some("uk")),
        ("ja-JP", Some("jp")),
        ("fil-PH", Some("ph")),
        ("", None),
        ("invariant", None),
    ];
    for (locale, expected) in cases {
        assert_eq!(tld_hint_for_locale(locale).as_deref(), expected, "{locale}");
    }
    let locale = system_locale_name();
    assert!(locale.len() >= 2, "locale name {locale:?}");
    assert_eq!(system_tld_hint(), tld_hint_for_locale(&locale));
}
