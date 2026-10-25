//! Access to resources that live outside this crate: local images, rendered math and diagrams.
//!
//! Math (MiTeX → Typst) and Mermaid rendering live in `ssw-render`; the exporters in this crate
//! only see them through [`AssetProvider`]. When the provider returns `None` the exporters fall
//! back to showing the source (math → `<code>` with the TeX source, Mermaid → a code block).

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// Supplies the external resources an exporter embeds.
pub trait AssetProvider {
    /// Bytes and MIME type of the image referenced as `path` (a destination exactly as written
    /// in the Markdown). `None` when the image cannot be loaded.
    fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)>;
    /// SVG markup for a TeX formula. `None` when rendering failed or is unavailable.
    fn math_svg(&self, latex: &str, display: bool) -> Option<String>;
    /// SVG markup for a Mermaid diagram. `None` when rendering failed or is unavailable.
    fn mermaid_svg(&self, source: &str) -> Option<String>;
}

/// A provider that has nothing: every lookup misses, so exporters use their source fallbacks.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAssets;

impl AssetProvider for NoAssets {
    fn image(&self, _path: &str) -> Option<(Vec<u8>, &'static str)> {
        None
    }
    fn math_svg(&self, _latex: &str, _display: bool) -> Option<String> {
        None
    }
    fn mermaid_svg(&self, _source: &str) -> Option<String> {
        None
    }
}

/// `data:` URI for `bytes` with the given MIME type.
pub fn data_uri(bytes: &[u8], mime: &str) -> String {
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

/// File extension (without dot) that goes with an image MIME type.
pub fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/svg+xml" => "svg",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/tiff" => "tif",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        _ => "bin",
    }
}

/// Extensions an imported picture keeps: the formats Word and OpenDocument documents embed.
const PICTURE_EXTENSIONS: [&str; 18] = [
    "png", "jpg", "jpeg", "gif", "bmp", "tif", "tiff", "webp", "svg", "ico", "emf", "wmf", "emz",
    "wmz", "pict", "wdp", "svm", "eps",
];

/// `ext` (lower case) when it names a picture format, else `bin`: the extension of a part name
/// inside a document package is not trusted to be a safe file extension.
pub(crate) fn picture_extension(ext: &str) -> &str {
    if PICTURE_EXTENSIONS.contains(&ext) {
        ext
    } else {
        "bin"
    }
}

/// MIME type guessed from a path's extension (case-insensitive); `None` for non-images.
pub fn image_mime_for_path(path: &str) -> Option<&'static str> {
    let name = path.split(['?', '#']).next().unwrap_or(path);
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "ico" => "image/x-icon",
        _ => return None,
    })
}

/// MIME type detected from the leading bytes of an image; `None` for unknown formats.
pub fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// True for destinations that are not local files (`http:`, `https:`, `data:`, `mailto:`, …).
pub fn is_external_url(dest: &str) -> bool {
    let Some((scheme, _)) = dest.split_once(':') else {
        return false;
    };
    // A Windows drive path such as `C:\x` has a one-letter "scheme" and is local.
    scheme.len() > 1
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uri_encodes_base64() {
        assert_eq!(data_uri(b"abc", "image/png"), "data:image/png;base64,YWJj");
    }

    #[test]
    fn no_assets_misses_everything() {
        let p = NoAssets;
        assert!(p.image("a.png").is_none());
        assert!(p.math_svg("x", false).is_none());
        assert!(p.mermaid_svg("graph TD").is_none());
    }

    #[test]
    fn picture_extensions_are_limited_to_image_formats() {
        for known in PICTURE_EXTENSIONS {
            assert_eq!(picture_extension(known), known);
        }
        for other in ["exe", "", "dat", "png/../x", "toolongextension", "p-g", "PNG"] {
            assert_eq!(picture_extension(other), "bin", "{other:?}");
        }
    }

    #[test]
    fn mime_extension_table() {
        let table = [
            ("image/png", "png"),
            ("image/jpeg", "jpg"),
            ("image/gif", "gif"),
            ("image/svg+xml", "svg"),
            ("image/webp", "webp"),
            ("image/bmp", "bmp"),
            ("image/tiff", "tif"),
            ("image/x-icon", "ico"),
            ("image/vnd.microsoft.icon", "ico"),
            ("application/x", "bin"),
        ];
        for (mime, ext) in table {
            assert_eq!(extension_for_mime(mime), ext, "{mime}");
        }
    }

    #[test]
    fn path_mime_table() {
        let table = [
            ("a.PNG", Some("image/png")),
            ("dir/a.jpeg?x=1", Some("image/jpeg")),
            ("a.jpg", Some("image/jpeg")),
            ("a.gif", Some("image/gif")),
            ("a.svg#frag", Some("image/svg+xml")),
            ("a.webp", Some("image/webp")),
            ("a.bmp", Some("image/bmp")),
            ("a.tiff", Some("image/tiff")),
            ("a.ico", Some("image/x-icon")),
            ("a.pdf", None),
            ("noext", None),
        ];
        for (path, mime) in table {
            assert_eq!(image_mime_for_path(path), mime, "{path}");
        }
    }

    #[test]
    fn sniffing_table() {
        let mut webp = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        webp.extend_from_slice(&[0; 4]);
        let table: [(Vec<u8>, Option<&str>); 8] = [
            (b"\x89PNG\r\n\x1a\nrest".to_vec(), Some("image/png")),
            (vec![0xFF, 0xD8, 0xFF, 0xE0], Some("image/jpeg")),
            (b"GIF89a..".to_vec(), Some("image/gif")),
            (b"GIF87a..".to_vec(), Some("image/gif")),
            (b"BM\0\0".to_vec(), Some("image/bmp")),
            (webp, Some("image/webp")),
            (b"RIFF".to_vec(), None),
            (b"hello".to_vec(), None),
        ];
        for (bytes, mime) in table {
            assert_eq!(sniff_image_mime(&bytes), mime);
        }
    }

    #[test]
    fn external_url_detection() {
        let table = [
            ("https://x.y/a.png", true),
            ("data:image/png;base64,AA", true),
            ("mailto:a@b.c", true),
            ("C:\\pics\\a.png", false),
            ("images/a.png", false),
            ("./a:b.png", false),
            ("1http://x", false),
            ("", false),
        ];
        for (dest, expected) in table {
            assert_eq!(is_external_url(dest), expected, "{dest}");
        }
    }
}
