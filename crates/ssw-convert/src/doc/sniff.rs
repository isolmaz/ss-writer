//! Recognizes what a file with a `.doc` extension really is when it is not an OLE container,
//! so the caller can route it to the right importer.

/// Name of the format `bytes` look like, or `None` for unrecognizable binary data.
///
/// Possible results: `"RTF"`, `"DOCX"` (any ZIP package), `"PDF"`, `"HTML"`, `"text"`.
pub fn sniff(bytes: &[u8]) -> Option<&'static str> {
    let head = &bytes[..bytes.len().min(512)];
    if head.starts_with(b"{\\rtf") {
        return Some("RTF");
    }
    if head.starts_with(b"PK\x03\x04") {
        return Some("DOCX");
    }
    if head.starts_with(b"%PDF") {
        return Some("PDF");
    }
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    if lower.contains("<html") || lower.contains("<!doctype html") || lower.contains("xmlns:w=") {
        return Some("HTML");
    }
    let printable = head
        .iter()
        .filter(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
        .count();
    if !head.is_empty() && printable * 10 > head.len() * 9 {
        return Some("text");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_formats() {
        let cases: [(&[u8], Option<&str>); 11] = [
            (b"{\\rtf1\\ansi}", Some("RTF")),
            (b"PK\x03\x04\x14\x00", Some("DOCX")),
            (b"%PDF-1.4\n", Some("PDF")),
            (b"  <HTML><BODY>", Some("HTML")),
            (
                b"<?xml version='1.0'?><w:document xmlns:w=\"x\">",
                Some("HTML"),
            ),
            (b"<!DOCTYPE HTML PUBLIC>", Some("HTML")),
            (b"hello world\nsecond line", Some("text")),
            (&[0u8; 64], None),
            (&[0x80, 0x81, 0x82, 0x83, 0x84, 0x00, 0x01], None),
            (b"", None),
            (b"{\\rt", Some("text")),
        ];
        for (input, want) in cases {
            let shown = String::from_utf8_lossy(input);
            assert_eq!(sniff(input), want, "{shown:?}");
        }
    }

    #[test]
    fn only_looks_at_the_head() {
        let mut v = vec![b'a'; 600];
        v[550] = 0;
        assert_eq!(sniff(&v), Some("text"));
    }
}
