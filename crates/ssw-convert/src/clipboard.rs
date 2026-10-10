//! The Windows `HTML Format` (CF_HTML) clipboard payload: building it from an HTML fragment and
//! reading it back.
//!
//! The payload is a header of `Key:Value` lines with fixed-width, zero-padded byte offsets into
//! the UTF-8 payload, followed by the HTML document. The fragment sits between
//! `<!--StartFragment-->` and `<!--EndFragment-->` markers.

/// Why a CF_HTML payload could not be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ClipboardError {
    #[error("the clipboard HTML has no CF_HTML header")]
    MissingHeader,
    #[error("the clipboard HTML header has an invalid {0} offset")]
    BadOffset(&'static str),
    #[error("the clipboard HTML is not valid UTF-8")]
    InvalidUtf8,
}

/// A parsed CF_HTML payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfHtml {
    /// The selected fragment (between `StartFragment` and `EndFragment`).
    pub fragment: String,
    /// The whole HTML document (`StartHTML`..`EndHTML`).
    pub html: String,
    /// `SourceURL` header, when present.
    pub source_url: Option<String>,
}

const PREFIX: &str = "<html>\r\n<body>\r\n<!--StartFragment-->";
const SUFFIX: &str = "<!--EndFragment-->\r\n</body>\r\n</html>";

/// Builds the CF_HTML payload (without the trailing NUL the clipboard format conventionally
/// carries) for `fragment`. Offsets count UTF-8 bytes and are exact.
pub fn build_cf_html(fragment: &str, source_url: Option<&str>) -> String {
    const HEADER_FIXED: usize = "Version:0.9\r\n".len()
        + "StartHTML:0000000000\r\n".len()
        + "EndHTML:0000000000\r\n".len()
        + "StartFragment:0000000000\r\n".len()
        + "EndFragment:0000000000\r\n".len();
    let source_line = source_url
        .map(|u| format!("SourceURL:{u}\r\n"))
        .unwrap_or_default();
    let start_html = HEADER_FIXED + source_line.len();
    let start_fragment = start_html + PREFIX.len();
    let end_fragment = start_fragment + fragment.len();
    let end_html = end_fragment + SUFFIX.len();
    format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n{source_line}{PREFIX}{fragment}{SUFFIX}"
    )
}

/// Parses a CF_HTML payload. A trailing NUL is ignored. When the header lacks fragment offsets
/// the `<!--StartFragment-->` markers are used, and without them the whole document is the
/// fragment.
pub fn parse_cf_html(data: &[u8]) -> Result<CfHtml, ClipboardError> {
    let data = match data.iter().rposition(|b| *b != 0) {
        Some(last) => &data[..=last],
        None => &[][..],
    };
    let mut offsets: [Option<usize>; 4] = [None; 4];
    let mut source_url = None;
    let mut saw_version = false;
    let mut pos = 0;
    while pos < data.len() {
        let line_end = data[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(data.len(), |i| pos + i);
        let line = &data[pos..line_end];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            break;
        };
        if line.first() == Some(&b'<') {
            break;
        }
        let key = &line[..colon];
        let value =
            std::str::from_utf8(&line[colon + 1..]).map_err(|_| ClipboardError::InvalidUtf8)?;
        let slot = match key {
            b"StartHTML" => Some(0),
            b"EndHTML" => Some(1),
            b"StartFragment" => Some(2),
            b"EndFragment" => Some(3),
            b"Version" => {
                saw_version = true;
                None
            }
            b"SourceURL" => {
                source_url = Some(value.trim().to_string());
                None
            }
            _ => None,
        };
        if let Some(i) = slot {
            let parsed = value.trim().parse::<usize>().map_err(|_| {
                ClipboardError::BadOffset(
                    ["StartHTML", "EndHTML", "StartFragment", "EndFragment"][i],
                )
            })?;
            offsets[i] = Some(parsed);
        }
        pos = line_end + 1;
    }
    if !saw_version {
        return Err(ClipboardError::MissingHeader);
    }
    let slice = |name: &'static str, start: usize, end: usize| -> Result<String, ClipboardError> {
        let bytes = data
            .get(start..end)
            .ok_or(ClipboardError::BadOffset(name))?;
        std::str::from_utf8(bytes)
            .map(str::to_string)
            .map_err(|_| ClipboardError::InvalidUtf8)
    };
    let html = match (offsets[0], offsets[1]) {
        (Some(s), Some(e)) => slice("StartHTML", s, e)?,
        (Some(s), None) => slice("StartHTML", s, data.len())?,
        _ => {
            let body = &data[pos.min(data.len())..];
            std::str::from_utf8(body)
                .map_err(|_| ClipboardError::InvalidUtf8)?
                .to_string()
        }
    };
    let fragment = match (offsets[2], offsets[3]) {
        (Some(s), Some(e)) => slice("StartFragment", s, e)?,
        _ => match (
            html.find("<!--StartFragment-->"),
            html.find("<!--EndFragment-->"),
        ) {
            (Some(s), Some(e)) if s + 20 <= e => html[s + 20..e].to_string(),
            _ => html.clone(),
        },
    };
    Ok(CfHtml {
        fragment,
        html,
        source_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPECTED: &str = "Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000185\r\nStartFragment:0000000141\r\nEndFragment:0000000149\r\n<html>\r\n<body>\r\n<!--StartFragment--><b>x</b><!--EndFragment-->\r\n</body>\r\n</html>";

    #[test]
    fn builds_exact_known_payload() {
        assert_eq!(build_cf_html("<b>x</b>", None), EXPECTED);
    }

    #[test]
    fn offsets_are_byte_exact_with_multibyte_text_and_source_url() {
        let fragment = "<p>ğüşiö 日本語 😀</p>";
        let url = "https://example.com/a?b=c:d";
        let payload = build_cf_html(fragment, Some(url));
        let bytes = payload.as_bytes();
        let field = |name: &str| -> usize {
            let key = format!("{name}:");
            let at = payload.find(&key).unwrap() + key.len();
            payload[at..at + 10].parse().unwrap()
        };
        assert_eq!(
            &bytes[field("StartHTML")..field("StartHTML") + 6],
            b"<html>"
        );
        assert_eq!(field("EndHTML"), bytes.len());
        assert_eq!(
            &bytes[field("StartFragment")..field("EndFragment")],
            fragment.as_bytes()
        );
        assert_eq!(
            &bytes[field("StartFragment") - 20..field("StartFragment")],
            b"<!--StartFragment-->"
        );
        assert_eq!(
            &bytes[field("EndFragment")..field("EndFragment") + 18],
            b"<!--EndFragment-->"
        );
        assert!(payload.contains(&format!("SourceURL:{url}\r\n")));
        assert_eq!(payload.matches("SourceURL").count(), 1);
    }

    #[test]
    fn round_trip_returns_fragment_html_and_url() {
        let fragment = "<p>ğ 日本 &amp; x</p>";
        let payload = build_cf_html(fragment, Some("https://e.com/x:y"));
        let parsed = parse_cf_html(payload.as_bytes()).unwrap();
        assert_eq!(parsed.fragment, fragment);
        assert_eq!(parsed.source_url.as_deref(), Some("https://e.com/x:y"));
        assert!(parsed.html.starts_with("<html>"));
        assert!(parsed.html.ends_with("</html>"));
        let without = parse_cf_html(build_cf_html(fragment, None).as_bytes()).unwrap();
        assert_eq!(without.source_url, None);
    }

    #[test]
    fn trailing_nul_is_ignored() {
        let mut bytes = EXPECTED.as_bytes().to_vec();
        bytes.push(0);
        bytes.push(0);
        assert_eq!(parse_cf_html(&bytes).unwrap().fragment, "<b>x</b>");
    }

    #[test]
    fn zero_offsets_are_honored_not_guessed() {
        let doc = "<html><body><!--StartFragment--><i>q</i><!--EndFragment--></body></html>";
        let header = "Version:1.0\nStartHTML:0000000000\nEndHTML:0000000000\nStartFragment:0000000000\nEndFragment:0000000000\n";
        let parsed = parse_cf_html(format!("{header}{doc}").as_bytes()).unwrap();
        assert_eq!(parsed.fragment, "");
        assert_eq!(parsed.html, "");
    }

    #[test]
    fn missing_offsets_fall_back_to_markers_or_whole_document() {
        let doc = "<html><body><!--StartFragment--><i>q</i><!--EndFragment--></body></html>";
        let marker =
            parse_cf_html(format!("Version:0.9\r\nSourceURL:x\r\n{doc}").as_bytes()).unwrap();
        assert_eq!(marker.fragment, "<i>q</i>");
        assert_eq!(marker.html, doc);
        let plain = "<p>hi</p>";
        let whole = parse_cf_html(format!("Version:0.9\r\n{plain}").as_bytes()).unwrap();
        assert_eq!(whole.fragment, plain);
        let start_only_payload = format!("Version:0.9\r\nStartHTML:0000000035\r\n{plain}");
        let start_only = parse_cf_html(start_only_payload.as_bytes()).unwrap();
        assert_eq!(start_only.html, plain);
        let reversed = "Version:0.9\r\n<!--EndFragment--><!--StartFragment-->";
        let rev = parse_cf_html(reversed.as_bytes()).unwrap();
        assert_eq!(rev.fragment, "<!--EndFragment--><!--StartFragment-->");
    }

    #[test]
    fn unknown_header_keys_are_skipped_and_markup_with_a_colon_ends_the_header() {
        let with_unknown =
            parse_cf_html(b"Version:0.9\r\nVendor:acme\r\nSourceURL: https://e.com \r\n<p>x</p>")
                .unwrap();
        assert_eq!(with_unknown.fragment, "<p>x</p>");
        assert_eq!(with_unknown.source_url.as_deref(), Some("https://e.com"));
        let link = "<a href=\"http://e.com\">l</a>";
        let parsed = parse_cf_html(format!("Version:0.9\r\n{link}").as_bytes()).unwrap();
        assert_eq!(parsed.html, link);
        assert_eq!(parsed.fragment, link);
    }

    #[test]
    fn rejects_invalid_payloads() {
        let table: Vec<(Vec<u8>, ClipboardError)> = vec![
            (b"<p>no header</p>".to_vec(), ClipboardError::MissingHeader),
            (Vec::new(), ClipboardError::MissingHeader),
            (vec![0, 0], ClipboardError::MissingHeader),
            (
                b"Version:0.9\r\nStartHTML:abc\r\n".to_vec(),
                ClipboardError::BadOffset("StartHTML"),
            ),
            (
                b"Version:0.9\r\nEndHTML:-1\r\n".to_vec(),
                ClipboardError::BadOffset("EndHTML"),
            ),
            (
                b"Version:0.9\r\nStartFragment:x\r\n".to_vec(),
                ClipboardError::BadOffset("StartFragment"),
            ),
            (
                b"Version:0.9\r\nEndFragment:\r\n".to_vec(),
                ClipboardError::BadOffset("EndFragment"),
            ),
            (
                b"Version:0.9\r\nStartHTML:0000000000\r\nEndHTML:0000009999\r\n".to_vec(),
                ClipboardError::BadOffset("StartHTML"),
            ),
            (
                b"Version:0.9\r\nStartFragment:0000000005\r\nEndFragment:0000000001\r\n<p>"
                    .to_vec(),
                ClipboardError::BadOffset("StartFragment"),
            ),
            (
                b"Version:0.9\r\nSourceURL:\xff\r\n".to_vec(),
                ClipboardError::InvalidUtf8,
            ),
            (
                b"Version:0.9\r\n<p>\xff</p>".to_vec(),
                ClipboardError::InvalidUtf8,
            ),
        ];
        for (data, expected) in table {
            assert_eq!(parse_cf_html(&data), Err(expected), "{data:?}");
        }
    }

    #[test]
    fn slicing_inside_a_multibyte_char_is_invalid_utf8() {
        let payload = "Version:0.9\r\nStartHTML:0000000056\r\nEndHTML:0000000058\r\n€";
        assert_eq!(
            parse_cf_html(payload.as_bytes()),
            Err(ClipboardError::InvalidUtf8)
        );
    }
}
