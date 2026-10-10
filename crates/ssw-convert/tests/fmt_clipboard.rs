//! CF_HTML clipboard payloads: byte-exact offsets and parsing of payloads written by other
//! applications.

use ssw_convert::clipboard::{ClipboardError, build_cf_html, parse_cf_html};

/// Value of the `key:` header line of a CF_HTML payload.
fn header(payload: &str, key: &str) -> usize {
    payload
        .lines()
        .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix(':')))
        .unwrap_or_else(|| panic!("no {key} in {payload:?}"))
        .parse()
        .unwrap()
}

#[test]
fn offsets_are_exact_utf8_byte_positions() {
    for (fragment, url) in [
        ("<b>plain</b>", None),
        (
            "<p>ç ğ ı ö ş ü — 日本語 😀</p>",
            Some("https://example.org/ä?q=1"),
        ),
        ("", Some("file:///C:/a b.html")),
    ] {
        let payload = build_cf_html(fragment, url);
        let bytes = payload.as_bytes();
        let start_html = header(&payload, "StartHTML");
        let end_html = header(&payload, "EndHTML");
        let start_fragment = header(&payload, "StartFragment");
        let end_fragment = header(&payload, "EndFragment");
        assert_eq!(&bytes[start_fragment..end_fragment], fragment.as_bytes());
        assert!(bytes[start_html..].starts_with(b"<html>"));
        assert_eq!(end_html, bytes.len(), "EndHTML is the end of the payload");
        assert_eq!(
            &bytes[start_fragment - 20..start_fragment],
            b"<!--StartFragment-->"
        );
        assert_eq!(
            &bytes[end_fragment..end_fragment + 18],
            b"<!--EndFragment-->"
        );
        // Fixed-width, zero-padded offsets.
        assert!(payload.contains(&format!("StartHTML:{start_html:010}\r\n")));
        assert_eq!(payload.contains("SourceURL:"), url.is_some());
        let parsed = parse_cf_html(bytes).unwrap();
        assert_eq!(parsed.fragment, fragment);
        assert_eq!(parsed.source_url.as_deref(), url);
        assert_eq!(parsed.html.as_bytes(), &bytes[start_html..end_html]);
    }
}

#[test]
fn a_trailing_nul_from_the_clipboard_is_ignored() {
    let mut payload = build_cf_html("<i>x</i>", None).into_bytes();
    payload.push(0);
    assert_eq!(parse_cf_html(&payload).unwrap().fragment, "<i>x</i>");
}

#[test]
fn payloads_from_other_applications_parse() {
    // Without fragment offsets the `StartFragment`/`EndFragment` markers decide.
    let html = "<html><body>\r\n<!--StartFragment--><p>hi</p><!--EndFragment-->\r\n</body></html>";
    let payload = format!("Version:1.0\r\nSourceURL:https://a.b/\r\n{html}");
    let parsed = parse_cf_html(payload.as_bytes()).unwrap();
    assert_eq!(parsed.fragment, "<p>hi</p>");
    assert_eq!(parsed.html, html);
    assert_eq!(parsed.source_url.as_deref(), Some("https://a.b/"));
    // A lone StartHTML offset runs to the end of the data.
    let head = "Version:0.9\r\nStartHTML:0000000035\r\n";
    assert_eq!(head.len(), 35);
    let parsed = parse_cf_html(format!("{head}{html}").as_bytes()).unwrap();
    assert_eq!(parsed.html, html);
    // No markers at all: the whole document is the fragment.
    let parsed = parse_cf_html(b"Version:0.9\r\n<b>x</b>").unwrap();
    assert_eq!(parsed.fragment, "<b>x</b>");
}

#[test]
fn damaged_payloads_are_rejected() {
    assert_eq!(
        parse_cf_html(b"just text"),
        Err(ClipboardError::MissingHeader)
    );
    assert_eq!(parse_cf_html(b""), Err(ClipboardError::MissingHeader));
    assert_eq!(
        parse_cf_html(b"Version:0.9\r\nStartHTML:xx\r\n<html>"),
        Err(ClipboardError::BadOffset("StartHTML"))
    );
    assert_eq!(
        parse_cf_html(b"Version:0.9\r\nStartHTML:0000000001\r\nEndHTML:0000009999\r\nx"),
        Err(ClipboardError::BadOffset("StartHTML"))
    );
    assert_eq!(
        parse_cf_html(b"Version:0.9\r\nSourceURL:\xff\xfe\r\n"),
        Err(ClipboardError::InvalidUtf8)
    );
}
