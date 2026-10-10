//! Markdown → ePub 3, written straight into a zip archive.
//!
//! Chapters are cut at level-1 headings (content before the first one forms its own chapter;
//! a document without level-1 headings is a single chapter). The table of contents lists the
//! chapters with their level 2–6 headings nested below. Local images become resources under
//! `images/`; math and diagrams are inline SVG.
//!
//! Layout: `mimetype` (stored, first), `META-INF/container.xml`, and under `OEBPS/` the package
//! document `content.opf`, the EPUB 3 navigation document `nav.xhtml`, the EPUB 2 `toc.ncx`,
//! `stylesheet.css`, one `chapter_N.xhtml` per chapter and `images/image-N.ext`.

use std::io::{Cursor, Seek, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use ssw_core::markdown::ast::{Block, BlockKind};
use ssw_core::markdown::parse_document;

use crate::assets::AssetProvider;
use crate::css;
use crate::html::{Flavor, HeadingInfo, ImageMode, ImageResource, Renderer, push_escaped};

/// Options of an ePub export.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpubOptions {
    /// Book title; defaults to the first level-1 heading, then the file name without extension.
    pub title: Option<String>,
    pub author: Option<String>,
    /// BCP-47 language tag; defaults to `en`.
    pub lang: Option<String>,
    /// The style sheet of the book.
    pub theme: css::Theme,
}

/// Why the archive could not be built.
#[derive(Debug, thiserror::Error)]
pub enum EpubError {
    #[error("ePub archive could not be written: {0}")]
    Zip(#[from] zip::result::ZipError),
}

struct Chapter {
    body: String,
    headings: Vec<HeadingInfo>,
    /// The chapter starts with a level-1 heading.
    titled: bool,
}

/// A chapter ready to be packaged.
struct Page {
    /// File name below `OEBPS/`.
    href: String,
    title: String,
    xhtml: String,
    /// Level 2–6 headings of the chapter: (level, `href#id`, text).
    toc: Vec<(u8, String, String)>,
}

/// Block index ranges of the chapters.
fn chapter_ranges(blocks: &[Block]) -> Vec<(usize, usize)> {
    let starts: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.heading_level() == Some(1))
        .map(|(i, _)| i)
        .collect();
    let Some(&first) = starts.first() else {
        return vec![(0, blocks.len())];
    };
    let mut ranges = Vec::new();
    let has_preamble = blocks[..first].iter().any(|b| {
        !matches!(
            b.kind,
            BlockKind::FrontMatter { .. } | BlockKind::LinkReferenceDefinition(_)
        )
    });
    if has_preamble {
        ranges.push((0, first));
    }
    for (n, start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(blocks.len());
        ranges.push((*start, end));
    }
    ranges
}

fn file_stem(file_name: &str) -> &str {
    let name = file_name.rsplit(['/', '\\']).next().unwrap_or(file_name);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

fn xhtml_page(title: &str, lang: &str, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 512);
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\" lang=\"");
    push_escaped(&mut out, lang, true);
    out.push_str("\" xml:lang=\"");
    push_escaped(&mut out, lang, true);
    out.push_str("\">\n<head>\n<meta charset=\"utf-8\" />\n<title>");
    push_escaped(&mut out, title, false);
    out.push_str("</title>\n<link rel=\"stylesheet\" type=\"text/css\" href=\"stylesheet.css\" />\n</head>\n<body>\n");
    out.push_str(body);
    out.push_str("</body>\n</html>\n");
    out
}

const CONTAINER_XML: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<container version=\"1.0\" xmlns=\"urn:oasis:names:tc:opendocument:xmlns:container\">\n<rootfiles>\n<rootfile full-path=\"OEBPS/content.opf\" media-type=\"application/oebps-package+xml\" />\n</rootfiles>\n</container>\n";

/// Book-level metadata of the package document.
struct Meta<'a> {
    title: &'a str,
    lang: &'a str,
    author: Option<&'a str>,
    uuid: &'a str,
    modified: &'a str,
}

/// FNV-1a over `parts` (each followed by a separator byte), from the given seed.
fn fnv1a(seed: u64, parts: &[&str]) -> u64 {
    let mut hash = seed;
    for part in parts {
        for byte in part.bytes().chain(std::iter::once(0xFF)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
    hash
}

/// A stable version-5-style UUID derived from `parts`: the same book always gets the same id.
fn book_uuid(parts: &[&str]) -> String {
    let high = fnv1a(0xCBF2_9CE4_8422_2325, parts);
    let low = fnv1a(0x9E37_79B9_7F4A_7C15, parts);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x50;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..16].concat()
    )
}

/// `YYYY-MM-DDThh:mm:ssZ` for seconds since the Unix epoch (proleptic Gregorian calendar).
fn utc_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Civil-from-days (H. Hinnant), shifted so that March 1st starts the year.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

fn package_opf(meta: &Meta<'_>, pages: &[Page], resources: &[ImageResource]) -> String {
    let mut o = String::new();
    o.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<package version=\"3.0\" xmlns=\"http://www.idpf.org/2007/opf\" unique-identifier=\"book-id\" xml:lang=\"");
    push_escaped(&mut o, meta.lang, true);
    o.push_str("\">\n<metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n<dc:identifier id=\"book-id\">urn:uuid:");
    push_escaped(&mut o, meta.uuid, false);
    o.push_str("</dc:identifier>\n<dc:title>");
    push_escaped(&mut o, meta.title, false);
    o.push_str("</dc:title>\n<dc:language>");
    push_escaped(&mut o, meta.lang, false);
    o.push_str("</dc:language>\n");
    if let Some(author) = meta.author {
        o.push_str("<dc:creator id=\"creator\">");
        push_escaped(&mut o, author, false);
        o.push_str("</dc:creator>\n<meta refines=\"#creator\" property=\"role\" scheme=\"marc:relators\">aut</meta>\n");
    }
    o.push_str("<meta property=\"dcterms:modified\">");
    push_escaped(&mut o, meta.modified, false);
    o.push_str("</meta>\n</metadata>\n<manifest>\n");
    o.push_str("<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\" />\n");
    o.push_str("<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\" />\n");
    o.push_str("<item id=\"css\" href=\"stylesheet.css\" media-type=\"text/css\" />\n");
    for (i, page) in pages.iter().enumerate() {
        o.push_str(&format!(
            "<item id=\"chapter_{}\" href=\"{}\" media-type=\"application/xhtml+xml\" />\n",
            i + 1,
            page.href
        ));
    }
    for (i, res) in resources.iter().enumerate() {
        o.push_str(&format!("<item id=\"image_{}\" href=\"", i + 1));
        push_escaped(&mut o, &res.path, true);
        o.push_str("\" media-type=\"");
        push_escaped(&mut o, res.mime, true);
        o.push_str("\" />\n");
    }
    o.push_str("</manifest>\n<spine toc=\"ncx\">\n");
    for i in 0..pages.len() {
        o.push_str(&format!("<itemref idref=\"chapter_{}\" />\n", i + 1));
    }
    o.push_str("</spine>\n</package>\n");
    o
}

/// Nested `<ol>` for `(level, href, text)` entries; a level can be at most one deeper than the
/// entry before it.
fn nav_list(entries: &[(u8, String, String)]) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    for (level, href, text) in entries {
        let level = usize::from(*level).clamp(1, depth + 1);
        while depth > level {
            out.push_str("</li>\n</ol>\n");
            depth -= 1;
        }
        if depth == level {
            out.push_str("</li>\n");
        } else if depth == 0 {
            out.push_str("<ol>\n");
            depth = 1;
        } else {
            out.push_str("\n<ol>\n");
            depth += 1;
        }
        out.push_str("<li><a href=\"");
        push_escaped(&mut out, href, true);
        out.push_str("\">");
        push_escaped(&mut out, text, false);
        out.push_str("</a>");
    }
    while depth > 0 {
        out.push_str("</li>\n</ol>\n");
        depth -= 1;
    }
    out
}

/// All table-of-contents entries: every chapter followed by its sub-headings.
fn toc_entries(pages: &[Page]) -> Vec<(u8, String, String)> {
    let mut entries = Vec::new();
    for page in pages {
        entries.push((1, page.href.clone(), page.title.clone()));
        entries.extend(page.toc.iter().cloned());
    }
    entries
}

fn nav_xhtml(meta: &Meta<'_>, pages: &[Page]) -> String {
    let mut body = String::from("<nav epub:type=\"toc\" id=\"toc\">\n");
    body.push_str(&nav_list(&toc_entries(pages)));
    body.push_str("</nav>\n<nav epub:type=\"landmarks\" hidden=\"hidden\">\n<ol>\n<li><a epub:type=\"bodymatter\" href=\"");
    // A book always has at least one chapter.
    push_escaped(&mut body, &pages[0].href, true);
    body.push_str("\">");
    push_escaped(&mut body, &pages[0].title, false);
    body.push_str("</a></li>\n</ol>\n</nav>\n");
    xhtml_page(meta.title, meta.lang, &body)
}

fn toc_ncx(meta: &Meta<'_>, pages: &[Page]) -> String {
    let mut o = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">\n<head>\n<meta name=\"dtb:uid\" content=\"urn:uuid:",
    );
    push_escaped(&mut o, meta.uuid, true);
    o.push_str("\" />\n<meta name=\"dtb:depth\" content=\"1\" />\n<meta name=\"dtb:totalPageCount\" content=\"0\" />\n<meta name=\"dtb:maxPageNumber\" content=\"0\" />\n</head>\n<docTitle><text>");
    push_escaped(&mut o, meta.title, false);
    o.push_str("</text></docTitle>\n<navMap>\n");
    for (i, (_, href, text)) in toc_entries(pages).iter().enumerate() {
        o.push_str(&format!(
            "<navPoint id=\"navpoint-{0}\" playOrder=\"{0}\"><navLabel><text>",
            i + 1
        ));
        push_escaped(&mut o, text, false);
        o.push_str("</text></navLabel><content src=\"");
        push_escaped(&mut o, href, true);
        o.push_str("\" /></navPoint>\n");
    }
    o.push_str("</navMap>\n</ncx>\n");
    o
}

/// One file of the archive.
struct Entry<'a> {
    name: String,
    /// Stored uncompressed (only `mimetype` must be) instead of deflated.
    stored: bool,
    bytes: &'a [u8],
}

impl<'a> Entry<'a> {
    fn new(name: &str, stored: bool, bytes: &'a [u8]) -> Self {
        Entry {
            name: name.to_string(),
            stored,
            bytes,
        }
    }
}

/// Zips `entries` in order into memory.
fn write_zip(entries: &[Entry<'_>]) -> Result<Vec<u8>, EpubError> {
    zip_into(Cursor::new(Vec::new()), entries).map(Cursor::into_inner)
}

/// Zips `entries` in order into `sink`. A fixed entry time keeps the archive reproducible.
fn zip_into<W: Write + Seek>(sink: W, entries: &[Entry<'_>]) -> Result<W, EpubError> {
    let base = SimpleFileOptions::default().last_modified_time(zip::DateTime::default());
    let mut zip = ZipWriter::new(sink);
    for entry in entries {
        let method = if entry.stored {
            CompressionMethod::Stored
        } else {
            CompressionMethod::Deflated
        };
        zip.start_file(entry.name.as_str(), base.compression_method(method))?;
        zip.write_all(entry.bytes)
            .map_err(zip::result::ZipError::Io)?;
    }
    Ok(zip.finish()?)
}

/// Builds the ePub for the Markdown `src`; `file_name` supplies the fallback title.
pub fn export_epub(
    src: &str,
    file_name: &str,
    provider: &dyn AssetProvider,
    opts: &EpubOptions,
) -> Result<Vec<u8>, EpubError> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    build(now, src, file_name, provider, opts)
}

/// [`export_epub`] stamped with `now` (seconds since the Unix epoch).
fn build(
    now: u64,
    src: &str,
    file_name: &str,
    provider: &dyn AssetProvider,
    opts: &EpubOptions,
) -> Result<Vec<u8>, EpubError> {
    let doc = parse_document(src);
    let mut renderer = Renderer::new(
        &doc.blocks,
        src,
        provider,
        Flavor::Xhtml,
        ImageMode::Resources,
    );
    let mut chapters = Vec::new();
    for (start, end) in chapter_ranges(&doc.blocks) {
        let before = renderer.headings.len();
        let mut body = String::new();
        renderer.blocks(&doc.blocks[start..end], &mut body);
        renderer.footnotes_section(&mut body);
        chapters.push(Chapter {
            body,
            headings: renderer.headings[before..].to_vec(),
            titled: doc.blocks.get(start).and_then(Block::heading_level) == Some(1),
        });
    }

    let first_h1 = renderer
        .headings
        .iter()
        .find(|h| h.level == 1 && !h.text.trim().is_empty())
        .map(|h| h.text.trim().to_string());
    let book_title = opts
        .title
        .clone()
        .or(first_h1)
        .unwrap_or_else(|| file_stem(file_name).to_string());
    let lang = opts.lang.as_deref().unwrap_or("en");

    let pages: Vec<Page> = chapters
        .iter()
        .enumerate()
        .map(|(i, chapter)| {
            let href = format!("chapter_{}.xhtml", i + 1);
            let title = match chapter.headings.first() {
                Some(h) if chapter.titled && !h.text.trim().is_empty() => h.text.trim().to_string(),
                _ => book_title.clone(),
            };
            let toc = chapter
                .headings
                .iter()
                .filter(|h| h.level >= 2)
                .map(|h| {
                    let text = if h.text.trim().is_empty() {
                        h.id.as_str()
                    } else {
                        h.text.trim()
                    };
                    (h.level, format!("{href}#{}", h.id), text.to_string())
                })
                .collect();
            let xhtml = xhtml_page(&title, lang, &chapter.body);
            Page {
                href,
                title,
                xhtml,
                toc,
            }
        })
        .collect();

    let mut identity: Vec<&str> = vec![book_title.as_str(), lang];
    identity.extend(pages.iter().map(|p| p.xhtml.as_str()));
    let uuid = book_uuid(&identity);
    let modified = utc_timestamp(now);
    let meta = Meta {
        title: &book_title,
        lang,
        author: opts.author.as_deref(),
        uuid: &uuid,
        modified: &modified,
    };

    let opf = package_opf(&meta, &pages, &renderer.resources);
    let ncx = toc_ncx(&meta, &pages);
    let nav = nav_xhtml(&meta, &pages);
    let entry = Entry::new;
    let mut entries = vec![
        entry("mimetype", true, b"application/epub+zip"),
        entry("META-INF/container.xml", false, CONTAINER_XML.as_bytes()),
        entry("OEBPS/content.opf", false, opf.as_bytes()),
        entry("OEBPS/toc.ncx", false, ncx.as_bytes()),
        entry("OEBPS/nav.xhtml", false, nav.as_bytes()),
        entry(
            "OEBPS/stylesheet.css",
            false,
            css::theme_css(opts.theme).as_bytes(),
        ),
    ];
    for page in &pages {
        entries.push(entry(
            &format!("OEBPS/{}", page.href),
            false,
            page.xhtml.as_bytes(),
        ));
    }
    for res in &renderer.resources {
        entries.push(entry(&format!("OEBPS/{}", res.path), false, &res.bytes));
    }
    write_zip(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::NoAssets;
    use crate::test_sink::Limited;
    use std::io::{Cursor, Read};

    struct Png;
    impl AssetProvider for Png {
        fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
            (path == "pic.png").then(|| (vec![0x89, b'P', b'N', b'G'], "image/png"))
        }
        fn math_svg(&self, _: &str, _: bool) -> Option<String> {
            None
        }
        fn mermaid_svg(&self, _: &str) -> Option<String> {
            None
        }
    }

    fn entries(epub: &[u8]) -> Vec<String> {
        let mut zip = zip::ZipArchive::new(Cursor::new(epub)).unwrap();
        (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().unwrap().into_owned())
            .collect()
    }

    fn read(epub: &[u8], name: &str) -> String {
        let mut zip = zip::ZipArchive::new(Cursor::new(epub)).unwrap();
        let mut file = zip.by_name(name).unwrap();
        let mut s = String::new();
        file.read_to_string(&mut s).unwrap();
        s
    }

    #[test]
    fn zip_structure_starts_with_uncompressed_mimetype() {
        let epub = export_epub("# T\n\nbody", "t.md", &NoAssets, &EpubOptions::default()).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(&epub[..])).unwrap();
        {
            let first = zip.by_index(0).unwrap();
            assert_eq!(first.name().unwrap(), "mimetype");
            assert_eq!(first.compression(), zip::CompressionMethod::Stored);
        }
        assert_eq!(read(&epub, "mimetype"), "application/epub+zip");
        let names = entries(&epub);
        for wanted in [
            "META-INF/container.xml",
            "OEBPS/content.opf",
            "OEBPS/toc.ncx",
            "OEBPS/nav.xhtml",
            "OEBPS/stylesheet.css",
            "OEBPS/chapter_1.xhtml",
        ] {
            assert!(names.iter().any(|n| n == wanted), "{wanted} in {names:?}");
        }
        assert!(read(&epub, "OEBPS/stylesheet.css").contains("Cascadia Mono"));
    }

    #[test]
    fn chapters_split_at_h1_and_toc_lists_headings() {
        let md =
            "intro text\n\n# One\n\nfirst\n\n## Sub A\n\n### Deep\n\n# Two\n\nsecond\n\n## Sub B";
        let epub = export_epub(md, "book.md", &NoAssets, &EpubOptions::default()).unwrap();
        let names = entries(&epub);
        for n in 1..=3 {
            assert!(
                names
                    .iter()
                    .any(|x| x == &format!("OEBPS/chapter_{n}.xhtml")),
                "{names:?}"
            );
        }
        assert!(!names.iter().any(|x| x == "OEBPS/chapter_4.xhtml"));
        let c1 = read(&epub, "OEBPS/chapter_1.xhtml");
        assert!(c1.contains("intro text") && !c1.contains("first"));
        let c2 = read(&epub, "OEBPS/chapter_2.xhtml");
        assert!(
            c2.contains("<h1 id=\"one\">One</h1>")
                && c2.contains("<h2 id=\"sub-a\">Sub A</h2>")
                && !c2.contains("second")
        );
        let c3 = read(&epub, "OEBPS/chapter_3.xhtml");
        assert!(c3.contains("second") && c3.contains("<title>Two</title>"));
        let nav = read(&epub, "OEBPS/nav.xhtml");
        for wanted in [
            "chapter_2.xhtml\">One<",
            "chapter_2.xhtml#sub-a\">Sub A<",
            "chapter_2.xhtml#deep\">Deep<",
            "chapter_3.xhtml#sub-b\">Sub B<",
        ] {
            assert!(nav.contains(wanted), "{wanted} in {nav}");
        }
        let opf = read(&epub, "OEBPS/content.opf");
        assert!(opf.contains("<dc:title>One</dc:title>"), "{opf}");
    }

    #[test]
    fn single_chapter_without_h1_uses_file_name_and_options() {
        let opts = EpubOptions {
            title: None,
            author: Some("Ada".into()),
            lang: Some("tr".into()),
            ..EpubOptions::default()
        };
        let epub = export_epub(
            "## Only sub\n\ntext",
            "C:\\docs\\my notes.md",
            &NoAssets,
            &opts,
        )
        .unwrap();
        let opf = read(&epub, "OEBPS/content.opf");
        assert!(opf.contains("<dc:title>my notes</dc:title>"), "{opf}");
        assert!(opf.contains("Ada"));
        assert!(opf.contains(">tr</dc:language>"), "{opf}");
        let c1 = read(&epub, "OEBPS/chapter_1.xhtml");
        assert!(c1.contains("lang=\"tr\" xml:lang=\"tr\""));
        assert!(!entries(&epub).iter().any(|n| n == "OEBPS/chapter_2.xhtml"));
    }

    #[test]
    fn explicit_title_wins_and_empty_heading_falls_back() {
        let opts = EpubOptions {
            title: Some("Given".into()),
            ..EpubOptions::default()
        };
        let epub = export_epub("#\n\n## \n\nx", "a.md", &NoAssets, &opts).unwrap();
        assert!(read(&epub, "OEBPS/content.opf").contains("<dc:title>Given</dc:title>"));
        let c1 = read(&epub, "OEBPS/chapter_1.xhtml");
        assert!(c1.contains("<title>Given</title>"), "{c1}");
    }

    #[test]
    fn front_matter_only_preamble_does_not_create_a_chapter() {
        let epub = export_epub(
            "---\ntitle: x\n---\n\n# A\n\ntext",
            "a.md",
            &NoAssets,
            &EpubOptions::default(),
        )
        .unwrap();
        let names = entries(&epub);
        assert!(names.iter().any(|n| n == "OEBPS/chapter_1.xhtml"));
        assert!(!names.iter().any(|n| n == "OEBPS/chapter_2.xhtml"));
        assert!(read(&epub, "OEBPS/chapter_1.xhtml").contains("<h1 id=\"a\">A</h1>"));
    }

    #[test]
    fn images_are_embedded_as_resources_and_footnotes_restart_per_chapter() {
        let md = "# A\n\n![p](pic.png) x[^1]\n\n[^1]: one\n\n# B\n\ny[^2]\n\n[^2]: two";
        let epub = export_epub(md, "a.md", &Png, &EpubOptions::default()).unwrap();
        assert!(
            entries(&epub)
                .iter()
                .any(|n| n == "OEBPS/images/image-1.png")
        );
        let mut zip = zip::ZipArchive::new(Cursor::new(&epub[..])).unwrap();
        let mut bytes = Vec::new();
        zip.by_name("OEBPS/images/image-1.png")
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, vec![0x89, b'P', b'N', b'G']);
        let c1 = read(&epub, "OEBPS/chapter_1.xhtml");
        assert!(c1.contains("<img src=\"images/image-1.png\" alt=\"p\" />"));
        assert!(c1.contains("id=\"fn-1\"") && c1.contains("one"));
        let c2 = read(&epub, "OEBPS/chapter_2.xhtml");
        assert!(c2.contains("id=\"fn-1\"") && c2.contains("two"));
    }

    #[test]
    fn empty_document_is_a_valid_single_chapter_book() {
        let epub = export_epub("", "empty.md", &NoAssets, &EpubOptions::default()).unwrap();
        assert!(entries(&epub).iter().any(|n| n == "OEBPS/chapter_1.xhtml"));
    }

    #[test]
    fn xhtml_pages_are_well_formed_xml() {
        let md = "# T\n\na  \nb\n\n---\n\n- [x] t\n\n![i](https://x/y.png)\n\n| a |\n|---|\n| & |";
        let epub = export_epub(md, "a.md", &NoAssets, &EpubOptions::default()).unwrap();
        let page = read(&epub, "OEBPS/chapter_1.xhtml");
        assert_eq!(xml_error(&page), None, "{page}");
    }

    /// The first well-formedness error of `xml`, if any.
    fn xml_error(xml: &str) -> Option<String> {
        let mut reader = quick_xml::Reader::from_str(xml);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Eof) => return None,
                Ok(_) => {}
                Err(e) => return Some(e.to_string()),
            }
        }
    }

    #[test]
    fn the_well_formedness_check_reports_mismatched_tags() {
        assert_eq!(xml_error("<a><b/></a>"), None);
        assert!(xml_error("<a><b></a>").is_some());
    }

    #[test]
    fn formulas_and_diagrams_without_svg_fall_back_to_their_source() {
        let md = "# T\n\n$x^2$\n\n$$\ny\n$$\n\n```mermaid\ngraph TD\n```\n";
        let epub = export_epub(md, "m.md", &Png, &EpubOptions::default()).unwrap();
        let page = read(&epub, "OEBPS/chapter_1.xhtml");
        assert!(!page.contains("<svg"), "{page}");
        assert!(page.contains("x^2") && page.contains("graph TD"), "{page}");
    }

    #[test]
    fn zip_errors_are_reported_not_panicked() {
        let entry = |name: &str| Entry {
            name: name.to_string(),
            stored: false,
            bytes: b"x",
        };
        let err = write_zip(&[entry("OEBPS/a.xhtml"), entry("OEBPS/a.xhtml")]).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("ePub archive could not be written: "),
            "{err}"
        );
        assert!(err.to_string().contains("Duplicate filename"), "{err}");
        let ok = write_zip(&[entry("mimetype"), entry("b")]).unwrap();
        assert!(ok.starts_with(b"PK\x03\x04"));
    }

    #[test]
    fn a_sink_that_gives_out_at_any_byte_fails_the_archive() {
        let entries = [
            Entry::new("mimetype", true, b"application/epub+zip"),
            Entry::new(
                "OEBPS/a.xhtml",
                false,
                b"<p>some text some text some text</p>",
            ),
        ];
        let full = write_zip(&entries).unwrap();
        // The failure lands in a header, in stored data, in the flush of deflated data or in the
        // central directory, depending on the budget; every one of them is reported.
        for budget in 0..full.len() as u64 {
            let sink = Limited {
                inner: Cursor::new(Vec::new()),
                budget,
            };
            let err = zip_into(sink, &entries).expect_err(&format!(
                "a budget of {budget} bytes cannot hold the archive"
            ));
            assert!(err.to_string().contains("sink is full"), "{budget}: {err}");
        }
        let sink = Limited {
            inner: Cursor::new(Vec::new()),
            budget: full.len() as u64,
        };
        assert_eq!(zip_into(sink, &entries).unwrap().inner.into_inner(), full);
    }

    #[test]
    fn export_is_deterministic_apart_from_the_modification_time() {
        let a = build(
            1_000_000_000,
            "# T\n\nx",
            "t.md",
            &NoAssets,
            &EpubOptions::default(),
        )
        .unwrap();
        let b = build(
            1_000_000_000,
            "# T\n\nx",
            "t.md",
            &NoAssets,
            &EpubOptions::default(),
        )
        .unwrap();
        assert_eq!(a, b);
        let epub = a;
        assert!(
            read(&epub, "OEBPS/content.opf")
                .contains("<meta property=\"dcterms:modified\">2001-09-09T01:46:40Z</meta>")
        );
    }

    #[test]
    fn timestamps_are_formatted_in_utc() {
        let table = [
            (0, "1970-01-01T00:00:00Z"),
            (86_399, "1970-01-01T23:59:59Z"),
            (951_782_400, "2000-02-29T00:00:00Z"),
            (1_000_000_000, "2001-09-09T01:46:40Z"),
            (1_709_164_800, "2024-02-29T00:00:00Z"),
            (1_709_251_199, "2024-02-29T23:59:59Z"),
            (4_102_444_800, "2100-01-01T00:00:00Z"),
        ];
        for (secs, expected) in table {
            assert_eq!(utc_timestamp(secs), expected, "{secs}");
        }
    }

    #[test]
    fn book_ids_are_stable_valid_uuids_that_follow_the_content() {
        let id = book_uuid(&["Title", "en", "<p>x</p>"]);
        assert_eq!(id, book_uuid(&["Title", "en", "<p>x</p>"]));
        assert_ne!(id, book_uuid(&["Title", "en", "<p>y</p>"]));
        // Part boundaries matter: moving a character between parts changes the id.
        assert_ne!(book_uuid(&["ab", "c"]), book_uuid(&["a", "bc"]));
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(
            id.chars().all(|c| c == '-' || c.is_ascii_hexdigit())
                && !id.contains(|c: char| c.is_ascii_uppercase())
        );
        assert!(groups[2].starts_with('5'), "{id}");
        // RFC 4122 variant: the two high bits of the group are `10`.
        let variant_digit = u8::from_str_radix(&groups[3][..1], 16).unwrap();
        assert_eq!(variant_digit >> 2, 0b10, "{id}");
    }

    #[test]
    fn navigation_lists_nest_by_heading_level() {
        let entry = |level: u8, href: &str, text: &str| (level, href.to_string(), text.to_string());
        let entries = [
            entry(1, "a.xhtml", "A"),
            entry(2, "a.xhtml#b", "B&"),
            entry(3, "a.xhtml#c", "C"),
            entry(2, "a.xhtml#d", "D"),
            entry(1, "b.xhtml", "E"),
        ];
        assert_eq!(
            nav_list(&entries),
            "<ol>\n<li><a href=\"a.xhtml\">A</a>\n<ol>\n<li><a href=\"a.xhtml#b\">B&amp;</a>\n<ol>\n<li><a href=\"a.xhtml#c\">C</a></li>\n</ol>\n</li>\n<li><a href=\"a.xhtml#d\">D</a></li>\n</ol>\n</li>\n<li><a href=\"b.xhtml\">E</a></li>\n</ol>\n"
        );
        // A heading cannot skip a level: h3 straight after a chapter nests one level down.
        assert_eq!(
            nav_list(&[entry(1, "a", "A"), entry(3, "a#c", "C")]),
            "<ol>\n<li><a href=\"a\">A</a>\n<ol>\n<li><a href=\"a#c\">C</a></li>\n</ol>\n</li>\n</ol>\n"
        );
        assert_eq!(nav_list(&[]), "");
    }

    #[test]
    fn metadata_is_escaped_in_every_package_document() {
        let opts = EpubOptions {
            title: Some("A <b> & \"c\"".into()),
            author: Some("O'Neil & <Co>".into()),
            lang: Some("en\"x".into()),
            ..EpubOptions::default()
        };
        let epub = export_epub("text", "t.md", &Png, &opts).unwrap();
        for name in [
            "OEBPS/content.opf",
            "OEBPS/toc.ncx",
            "OEBPS/nav.xhtml",
            "OEBPS/chapter_1.xhtml",
        ] {
            let xml = read(&epub, name);
            assert_eq!(xml_error(&xml), None, "{name}\n{xml}");
        }
        let opf = read(&epub, "OEBPS/content.opf");
        assert!(
            opf.contains("<dc:title>A &lt;b&gt; &amp; \"c\"</dc:title>"),
            "{opf}"
        );
        assert!(
            opf.contains("<dc:creator id=\"creator\">O'Neil &amp; &lt;Co&gt;</dc:creator>"),
            "{opf}"
        );
        assert!(opf.contains("xml:lang=\"en&quot;x\""), "{opf}");
        assert!(read(&epub, "OEBPS/toc.ncx").contains("<text>A &lt;b&gt; &amp; \"c\"</text>"));
    }

    #[test]
    fn images_appear_in_the_manifest_with_their_mime_type() {
        let epub = export_epub("![p](pic.png)", "t.md", &Png, &EpubOptions::default()).unwrap();
        let opf = read(&epub, "OEBPS/content.opf");
        assert!(
            opf.contains(
                "<item id=\"image_1\" href=\"images/image-1.png\" media-type=\"image/png\" />"
            ),
            "{opf}"
        );
        assert!(opf.contains("<itemref idref=\"chapter_1\" />"), "{opf}");
    }

    #[test]
    fn file_stems() {
        let table = [
            ("a.md", "a"),
            ("dir/b.c.md", "b.c"),
            ("C:\\x\\y.txt", "y"),
            (".hidden", ".hidden"),
            ("noext", "noext"),
        ];
        for (input, expected) in table {
            assert_eq!(file_stem(input), expected);
        }
    }
}
