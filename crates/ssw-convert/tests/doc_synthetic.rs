//! Legacy `.doc` rejection paths: encryption, old Word versions, damaged containers and files
//! that are not OLE at all. Third-party fixtures are used wherever one exists; only the
//! structural damage that no fixture has is built here, with the `cfb` crate.

use std::io::{Cursor, Write};
use std::path::PathBuf;

use ssw_convert::doc::{self, sniff};
use ssw_convert::import::ImportError;

fn fixture(dir: &str, name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures")
        .join(dir)
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// An OLE container holding the given streams (`/Name`).
fn container(streams: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cf = cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
    for (name, data) in streams {
        let mut stream = cf.create_stream(name).unwrap();
        stream.write_all(data).unwrap();
    }
    cf.flush().unwrap();
    cf.into_inner().into_inner()
}

/// The smallest FIB the reader accepts: Word 97+ header with no (fc, lcb) pairs.
fn fib(ident: u16, nfib: u16, flags: u16) -> Vec<u8> {
    let mut w = vec![0u8; 32];
    w[0..2].copy_from_slice(&ident.to_le_bytes());
    w[2..4].copy_from_slice(&nfib.to_le_bytes());
    w[6..8].copy_from_slice(&0x0409u16.to_le_bytes());
    w[10..12].copy_from_slice(&flags.to_le_bytes());
    w.extend_from_slice(&14u16.to_le_bytes());
    w.extend_from_slice(&[0u8; 28]);
    w.extend_from_slice(&22u16.to_le_bytes());
    w.extend_from_slice(&[0u8; 88]);
    w.extend_from_slice(&0u16.to_le_bytes());
    w
}

fn word_document(ident: u16, nfib: u16, flags: u16) -> Vec<u8> {
    container(&[("/WordDocument", &fib(ident, nfib, flags))])
}

#[test]
fn encrypted_fixture_is_refused() {
    assert_eq!(
        doc::import(&fixture("doc", "PasswordProtected.doc"), "a"),
        Err(ImportError::Encrypted)
    );
}

#[test]
fn encryption_flags_in_the_fib_are_refused() {
    for flags in [0x0100, 0x8000, 0x8100] {
        assert_eq!(
            doc::import(&word_document(0xA5EC, 0x00C1, flags), "a"),
            Err(ImportError::Encrypted),
            "{flags:#06x}"
        );
    }
}

#[test]
fn word_6_and_95_fixtures_are_unsupported_versions() {
    for name in ["Word6.doc", "Word95.doc"] {
        match doc::import(&fixture("doc", name), "a") {
            Err(ImportError::UnsupportedVersion(m)) => assert!(m.contains("Word"), "{name}: {m}"),
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn other_word_generations_are_unsupported_versions() {
    let cases: [(u16, u16, &str); 4] = [
        (0xA5DC, 0x0065, "Word 6/95"),
        (0xA5DB, 0x0023, "Word 2.0"),
        (0xA5EC, 0x0068, "Word binary"),
        (0xA5EC, 0x00C0, "Word binary"),
    ];
    for (ident, nfib, expect) in cases {
        match doc::import(&word_document(ident, nfib, 0), "a") {
            Err(ImportError::UnsupportedVersion(m)) => assert!(m.contains(expect), "{m}"),
            other => panic!("{ident:#06x}/{nfib:#06x}: {other:?}"),
        }
    }
}

#[test]
fn containers_that_are_not_word_documents_are_corrupt() {
    // not a Word identifier
    assert!(matches!(
        doc::import(&word_document(0x1234, 0x00C1, 0), "a"),
        Err(ImportError::Corrupt(_))
    ));
    // no WordDocument stream at all
    assert!(matches!(
        doc::import(&container(&[("/Other", b"x")]), "a"),
        Err(ImportError::Corrupt(_))
    ));
    // a FIB without the table stream it points at
    assert!(matches!(
        doc::import(&word_document(0xA5EC, 0x00C1, 0), "a"),
        Err(ImportError::Corrupt(_))
    ));
    assert!(matches!(
        doc::import(&word_document(0xA5EC, 0x00C1, 0x0200), "a"),
        Err(ImportError::Corrupt(_))
    ));
    // a FIB cut off in the middle
    let mut short = fib(0xA5EC, 0x00C1, 0);
    short.truncate(20);
    assert!(matches!(
        doc::import(&container(&[("/WordDocument", &short)]), "a"),
        Err(ImportError::Corrupt(_))
    ));
}

#[test]
fn a_well_formed_container_with_empty_tables_never_panics() {
    for flags in [0u16, 0x0200] {
        let bytes = container(&[
            ("/WordDocument", &fib(0xA5EC, 0x00C1, flags)),
            ("/0Table", b""),
            ("/1Table", b""),
        ]);
        match doc::import(&bytes, "a") {
            Ok(_) | Err(ImportError::Corrupt(_)) => {}
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn damaged_headers_are_corrupt_not_fatal() {
    let magic: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    // a header cut off before the end of its sector
    let mut short = magic.to_vec();
    short.extend_from_slice(&[0; 92]);
    assert!(matches!(
        doc::import(&short, "a"),
        Err(ImportError::Corrupt(_))
    ));
    // a complete but empty header block
    let mut zeros = magic.to_vec();
    zeros.extend_from_slice(&[0; 600]);
    assert!(matches!(
        doc::import(&zeros, "a"),
        Err(ImportError::Corrupt(_))
    ));
    // real documents with damaged header fields: a refusal or a salvage, never a panic
    let original = fixture("doc", "simple-list.doc");
    let damage: [(usize, &[u8]); 5] = [
        (0x1E, &[0xFF, 0xFF]),
        (0x1E, &[0x02, 0x00]),
        (0x2C, &[0xFF, 0xFF, 0xFF, 0x7F]),
        (0x30, &[0xFF, 0xFF, 0xFF, 0xFF]),
        (0x4C, &[0xFF; 16]),
    ];
    for (at, bytes) in damage {
        let mut copy = original.clone();
        copy[at..at + bytes.len()].copy_from_slice(bytes);
        match doc::import(&copy, "a") {
            Ok(_) | Err(ImportError::Corrupt(_)) => {}
            other => panic!("{at:#x}: {other:?}"),
        }
    }
    // a document cut off inside the header
    assert!(matches!(
        doc::import(&original[..100], "a"),
        Err(ImportError::Corrupt(_))
    ));
}

#[test]
fn files_that_are_not_ole_are_routed_by_their_signature() {
    let docx = fixture("docx", "mammoth_empty.docx");
    let odt = fixture("odt", "testStyles.odt");
    let not_a_zip = fixture("odt", "testODTnotaZipFile.odt");
    let rtf = fixture("rtf", "testRTFBoldItalic.rtf");
    let html = fixture("html", "lexical-word.html");
    let cases: [(&[u8], &str); 5] = [
        (&docx, "DOCX"),
        (&odt, "DOCX"),
        (&not_a_zip, "text"),
        (&rtf, "RTF"),
        (&html, "HTML"),
    ];
    for (bytes, detected) in cases {
        assert_eq!(sniff::sniff(bytes), Some(detected));
        assert_eq!(
            doc::import(bytes, "a"),
            Err(ImportError::WrongFormat {
                expected: "DOC",
                detected
            })
        );
    }
    assert_eq!(
        doc::import(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n", "a"),
        Err(ImportError::WrongFormat {
            expected: "DOC",
            detected: "PDF"
        })
    );
}

#[test]
fn unrecognizable_binary_data_is_corrupt() {
    for bytes in [
        &[0u8; 64][..],
        &[0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88][..],
        b"",
        b"\xD0\xCF",
    ] {
        assert!(
            matches!(doc::import(bytes, "a"), Err(ImportError::Corrupt(_))),
            "{bytes:?}"
        );
    }
}

#[test]
fn word_2_signature_is_an_unsupported_version() {
    let bytes = [0xDB, 0xA5, 0, 1, 2, 3, 4, 5, 6, 7, 8];
    assert_eq!(
        doc::import(&bytes, "a"),
        Err(ImportError::UnsupportedVersion("Word 2.0".into()))
    );
}
