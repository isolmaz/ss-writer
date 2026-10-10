//! Export integration: single-file HTML, ePub packages (parsed back as XML) and plain text.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesStart, Event};
use ssw_convert::assets::{AssetProvider, NoAssets};
use ssw_convert::clipboard::{build_cf_html, parse_cf_html};
use ssw_convert::css::{Theme, dark_theme_css, light_theme_css};
use ssw_convert::epub::{EpubOptions, export_epub};
use ssw_convert::html::{HtmlOptions, render_document, render_fragment};
use ssw_convert::txt::markdown_to_text;

const PNG_HEADER: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Serves one PNG (`logo.png`) and renders math and diagrams as recognizable SVG.
struct Provider;

impl AssetProvider for Provider {
    fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
        (path == "logo.png").then(|| (PNG_HEADER.to_vec(), "image/png"))
    }
    fn math_svg(&self, latex: &str, display: bool) -> Option<String> {
        Some(format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" data-display=\"{display}\"><title>{latex}</title></svg>"
        ))
    }
    fn mermaid_svg(&self, source: &str) -> Option<String> {
        Some(format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" data-kind=\"mermaid\"><desc>{source}</desc></svg>"
        ))
    }
}

/// The content of `<main>…</main>` of an exported document.
fn main_of(doc: &str) -> &str {
    let start = doc.find("<main>\n").expect("<main> in document") + "<main>\n".len();
    let end = doc.rfind("</main>").expect("</main> in document");
    &doc[start..end]
}

fn body_of(md: &str) -> String {
    main_of(&render_document(md, &NoAssets, &HtmlOptions::default())).to_string()
}

// ---------------------------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------------------------

#[test]
fn html_export_is_one_self_contained_file() {
    let md = "# Report\n\nInline $x^2$ and an image ![logo](logo.png \"The logo\").\n\n$$\nx^2 + y^2\n$$\n\n```mermaid\ngraph TD\n  A-->B\n```\n";
    let doc = render_document(md, &Provider, &HtmlOptions::default());
    assert!(
        doc.starts_with("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">"),
        "{doc}"
    );
    assert!(doc.contains("<title>Report</title>"));
    // The whole style sheet travels inside the file.
    assert_eq!(doc.matches("<style>").count(), 1);
    assert!(doc.contains(light_theme_css()));
    // Images are base64 data URIs of the exact bytes (the PNG signature encodes to `iVBORw0KGgo=`).
    assert!(
        doc.contains(
            "<img src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"logo\" title=\"The logo\">"
        ),
        "{doc}"
    );
    // Math and diagrams are inline SVG supplied by the provider.
    assert!(doc.contains("<span class=\"math inline\"><svg xmlns=\"http://www.w3.org/2000/svg\" data-display=\"false\"><title>x^2</title></svg></span>"));
    assert!(doc.contains("<div class=\"math display\"><svg xmlns=\"http://www.w3.org/2000/svg\" data-display=\"true\"><title>x^2 + y^2</title></svg></div>"));
    assert!(doc.contains("<div class=\"mermaid-diagram\"><svg xmlns=\"http://www.w3.org/2000/svg\" data-kind=\"mermaid\"><desc>graph TD\n  A-->B</desc></svg></div>"));
    // Nothing is fetched from elsewhere and nothing executes.
    for forbidden in [
        "<link",
        "<script",
        "src=\"logo.png\"",
        "http://",
        "https://",
    ] {
        assert!(
            !doc.replace("http://www.w3.org/2000/svg", "")
                .contains(forbidden),
            "{forbidden}"
        );
    }
    assert!(doc.ends_with("</main>\n</body>\n</html>\n"));
}

#[test]
fn html_export_without_provider_shows_sources() {
    let body =
        body_of("$a<b$ and\n\n$$\nx^2\n$$\n\n```mermaid\ngraph TD\n```\n\n![gone](missing.png)");
    assert_eq!(
        body,
        "<p><code class=\"math\">a&lt;b</code> and</p>\n<pre class=\"math\"><code>x^2</code></pre>\n<pre><code class=\"language-mermaid\">graph TD\n</code></pre>\n<p><img src=\"missing.png\" alt=\"gone\"></p>\n"
    );
}

#[test]
fn html_export_escapes_every_special_character() {
    let table: &[(&str, &str)] = &[
        (
            "Tom & Jerry say 1 < 2 > 0 \"q\" 'a'",
            "<p>Tom &amp; Jerry say 1 &lt; 2 &gt; 0 \"q\" 'a'</p>\n",
        ),
        ("`<&>\"`", "<p><code>&lt;&amp;&gt;\"</code></p>\n"),
        ("&lt;script&gt;", "<p>&lt;script&gt;</p>\n"),
        ("a\\<b\\>", "<p>a&lt;b&gt;</p>\n"),
        (
            "```html\n<a href=\"x\">&amp;</a>\n```",
            "<pre><code class=\"language-html\">&lt;a href=\"x\"&gt;&amp;amp;&lt;/a&gt;\n</code></pre>\n",
        ),
        (
            "[x](http://e.com/?a=1&b=2 \"T <b> & 'q'\")",
            "<p><a href=\"http://e.com/?a=1&amp;b=2\" title=\"T &lt;b&gt; &amp; 'q'\">x</a></p>\n",
        ),
        (
            "![a \"b\" & c](x.png)",
            "<p><img src=\"x.png\" alt=\"a &quot;b&quot; &amp; c\"></p>\n",
        ),
        (
            "<script>alert(\"x\")</script>",
            "<pre class=\"raw-html\">&lt;script&gt;alert(\"x\")&lt;/script&gt;</pre>\n",
        ),
        (
            "x <b>y</b> <span onclick=\"e()\">z</span>",
            "<p>x &lt;b&gt;y&lt;/b&gt; &lt;span onclick=\"e()\"&gt;z&lt;/span&gt;</p>\n",
        ),
        ("[a](javascript:alert(1))", "<p><a href=\"#\">a</a></p>\n"),
        ("# R&D \"x\"", "<h1 id=\"rd-x\">R&amp;D \"x\"</h1>\n"),
        (
            "[[Q&A]]",
            "<p><a class=\"wikilink\" href=\"Q%26A.html\">Q&amp;A</a></p>\n",
        ),
        (
            "> [!NOTE] R&D team\n> body",
            "<div class=\"callout callout-note\">\n<p class=\"callout-title\">R&amp;D team</p>\n<p>body</p>\n</div>\n",
        ),
        (
            "| a&b | <c> |\n|---|---|\n| \"q\" | 'x' |",
            "<table>\n<thead>\n<tr><th>a&amp;b</th><th>&lt;c&gt;</th></tr>\n</thead>\n<tbody>\n<tr><td>\"q\"</td><td>'x'</td></tr>\n</tbody>\n</table>\n",
        ),
    ];
    for (md, expected) in table {
        assert_eq!(body_of(md), *expected, "{md:?}");
    }
    let doc = render_document("# A & B <c>\n\ntext", &NoAssets, &HtmlOptions::default());
    assert!(doc.contains("<title>A &amp; B &lt;c&gt;</title>"), "{doc}");
    let opts = HtmlOptions {
        title: Some("T\"<&>".into()),
        lang: Some("tr\"><x".into()),
        ..HtmlOptions::default()
    };
    let doc = render_document("x", &NoAssets, &opts);
    assert!(doc.contains("<html lang=\"tr&quot;&gt;&lt;x\">"), "{doc}");
    assert!(doc.contains("<title>T\"&lt;&amp;&gt;</title>"), "{doc}");
}

#[test]
fn html_export_gives_headings_unique_readable_ids() {
    let md = "# Intro\n\n## Intro\n\n### Ünïcode Ñame!\n\n# 日本語 見出し\n\n## Hello *World* & `code`\n\n#";
    assert_eq!(
        body_of(md),
        "<h1 id=\"intro\">Intro</h1>\n<h2 id=\"intro-1\">Intro</h2>\n<h3 id=\"ünïcode-ñame\">Ünïcode Ñame!</h3>\n<h1 id=\"日本語-見出し\">日本語 見出し</h1>\n<h2 id=\"hello-world-code\">Hello <em>World</em> &amp; <code>code</code></h2>\n<h1 id=\"section\"></h1>\n"
    );
}

#[test]
fn html_export_renders_task_lists_as_disabled_checkboxes() {
    assert_eq!(
        body_of("- [ ] open\n- [x] done\n- plain"),
        "<ul>\n<li class=\"task-list-item\"><input type=\"checkbox\" disabled> open</li>\n<li class=\"task-list-item done\"><input type=\"checkbox\" disabled checked> done</li>\n<li>plain</li>\n</ul>\n"
    );
    assert!(light_theme_css().contains("li.task-list-item"));
}

#[test]
fn html_export_renders_every_callout_kind() {
    for (keyword, class, title) in [
        ("NOTE", "note", "Note"),
        ("TIP", "tip", "Tip"),
        ("IMPORTANT", "important", "Important"),
        ("WARNING", "warning", "Warning"),
        ("CAUTION", "caution", "Caution"),
    ] {
        let expected = format!(
            "<div class=\"callout callout-{class}\">\n<p class=\"callout-title\">{title}</p>\n<p>body <em>{class}</em></p>\n</div>\n"
        );
        assert_eq!(
            body_of(&format!("> [!{keyword}]\n> body *{class}*")),
            expected,
            "{keyword}"
        );
        if class != "note" {
            assert!(
                light_theme_css().contains(&format!(".callout-{class}{{")),
                "{class}"
            );
        }
    }
}

#[test]
fn html_export_renders_footnotes_in_order_of_first_reference() {
    let md = "Alpha[^n] beta[^m].\n\n[^m]: Second *note* with a < b & c.\n\n[^n]: First.";
    assert_eq!(
        body_of(md),
        "<p>Alpha<sup class=\"footnote-ref\"><a href=\"#fn-1\" id=\"fnref-1\">1</a></sup> beta<sup class=\"footnote-ref\"><a href=\"#fn-2\" id=\"fnref-2\">2</a></sup>.</p>\n<section class=\"footnotes\">\n<hr>\n<ol>\n<li id=\"fn-1\">\n<p>First. <a href=\"#fnref-1\" class=\"footnote-backref\">\u{21A9}</a></p>\n</li>\n<li id=\"fn-2\">\n<p>Second <em>note</em> with a &lt; b &amp; c. <a href=\"#fnref-2\" class=\"footnote-backref\">\u{21A9}</a></p>\n</li>\n</ol>\n</section>\n"
    );
}

#[test]
fn html_export_renders_table_alignment_and_cell_markup() {
    let md = "| Left | Center | Right | Default |\n|:--|:-:|--:|---|\n| **b** | `c<` | [r](http://e.org) | d & e |";
    assert_eq!(
        body_of(md),
        "<table>\n<thead>\n<tr><th style=\"text-align:left\">Left</th><th style=\"text-align:center\">Center</th><th style=\"text-align:right\">Right</th><th>Default</th></tr>\n</thead>\n<tbody>\n<tr><td style=\"text-align:left\"><strong>b</strong></td><td style=\"text-align:center\"><code>c&lt;</code></td><td style=\"text-align:right\"><a href=\"http://e.org\">r</a></td><td>d &amp; e</td></tr>\n</tbody>\n</table>\n"
    );
}

#[test]
fn html_export_renders_highlights_underline_and_page_breaks() {
    assert_eq!(
        body_of("==a== =={red}b== =={purple}c== ~u~ ~~s~~\n\n<!-- pagebreak -->\n\n---"),
        "<p><mark>a</mark> <mark class=\"hl-red\">b</mark> <mark class=\"hl-purple\">c</mark> <u>u</u> <del>s</del></p>\n<div class=\"page-break\"></div>\n<hr>\n"
    );
    let css = light_theme_css();
    assert!(css.contains("mark.hl-purple{background:#E3D0F8}"));
    assert!(css.contains(".page-break{break-after:page"));
}

#[test]
fn a_dark_html_export_embeds_the_dark_sheet_and_the_light_one_does_not() {
    let dark = HtmlOptions {
        theme: Theme::Dark,
        ..HtmlOptions::default()
    };
    let doc = render_document("# T\n\n> [!TIP]\n> x\n\n==m==", &NoAssets, &dark);
    assert_eq!(doc.matches("<style>").count(), 1);
    assert!(doc.contains(dark_theme_css()), "{doc}");
    assert!(doc.contains("color-scheme:dark"));
    assert!(doc.contains("body{background:#272727;color:#E4E4E4}"));
    let light = render_document("# T", &NoAssets, &HtmlOptions::default());
    assert!(!light.contains("color-scheme:dark") && !light.contains("#272727"));
    // The body of the document is the same in both themes.
    assert_eq!(
        body_of("# T\n\ntext"),
        main_of(&render_document("# T\n\ntext", &NoAssets, &dark))
    );
}

#[test]
fn a_dark_epub_carries_the_dark_sheet() {
    let opts = EpubOptions {
        theme: Theme::Dark,
        ..EpubOptions::default()
    };
    let mut archive = open(export_epub("# T\n\nx", "t.md", &NoAssets, &opts).unwrap());
    assert_eq!(
        read_string(&mut archive, "OEBPS/stylesheet.css"),
        dark_theme_css()
    );
}

#[test]
fn html_fragment_round_trips_through_cf_html() {
    let fragment = render_fragment("# Çağrı\n\n==yes== `x` — 日本語", &NoAssets);
    assert!(!fragment.contains("<html") && !fragment.contains("<style"));
    assert!(
        fragment.contains("<mark style=\"background-color:#FFF3A3\">yes</mark>"),
        "{fragment}"
    );
    let payload = build_cf_html(&fragment, Some("https://example.org/doc"));
    let parsed = parse_cf_html(payload.as_bytes()).unwrap();
    assert_eq!(parsed.fragment, fragment);
    assert_eq!(
        parsed.source_url.as_deref(),
        Some("https://example.org/doc")
    );
}

// ---------------------------------------------------------------------------------------------
// ePub
// ---------------------------------------------------------------------------------------------

type Archive = zip::ZipArchive<Cursor<Vec<u8>>>;

fn open(epub: Vec<u8>) -> Archive {
    zip::ZipArchive::new(Cursor::new(epub)).expect("ePub is a zip archive")
}

fn names(archive: &mut Archive) -> Vec<String> {
    (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().unwrap().into_owned())
        .collect()
}

fn read_bytes(archive: &mut Archive, name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

fn read_string(archive: &mut Archive, name: &str) -> String {
    String::from_utf8(read_bytes(archive, name)).unwrap()
}

#[derive(Debug, Clone, Default)]
struct El {
    name: String,
    attrs: Vec<(String, String)>,
    text: String,
}

impl El {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

fn element(start: &BytesStart<'_>) -> El {
    El {
        name: start.local_name().into_inner().to_string(),
        attrs: start
            .attributes()
            .map(|a| {
                let a = a.unwrap();
                (
                    a.key.local_name().into_inner().to_string(),
                    a.normalized_value(XmlVersion::Implicit1_0)
                        .unwrap()
                        .into_owned(),
                )
            })
            .collect(),
        text: String::new(),
    }
}

/// Parses `xml` strictly (end names are checked) and returns its elements in document order with
/// their direct text; panics when the document is not well-formed or has no single root.
fn parse_xml(xml: &str) -> Vec<El> {
    let mut reader = Reader::from_str(xml);
    let mut elements: Vec<El> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut roots = 0;
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                roots += usize::from(open.is_empty());
                elements.push(element(&e));
                open.push(elements.len() - 1);
            }
            Ok(Event::Empty(e)) => {
                roots += usize::from(open.is_empty());
                elements.push(element(&e));
            }
            Ok(Event::End(_)) => {
                open.pop();
            }
            Ok(Event::Text(t)) => {
                if let Some(&i) = open.last() {
                    elements[i].text.push_str(&t.xml10_content());
                }
            }
            Ok(Event::GeneralRef(r)) => {
                let name = r.xml10_content();
                let resolved = match resolve_predefined_entity(&name) {
                    Some(s) => s.to_string(),
                    None => r
                        .resolve_char_ref()
                        .ok()
                        .flatten()
                        .map(String::from)
                        .unwrap_or_else(|| panic!("unknown entity &{name};")),
                };
                if let Some(&i) = open.last() {
                    elements[i].text.push_str(&resolved);
                }
            }
            Ok(_) => {}
            Err(e) => panic!("not well-formed: {e}\n{xml}"),
        }
    }
    assert!(open.is_empty(), "unclosed elements in {xml}");
    assert_eq!(roots, 1, "exactly one root element in {xml}");
    elements
}

const BOOK: &str = "# Field Notes\n\nIntro with $x^2$ and a  \nhard break and ![logo](logo.png \"Logo\") & ![ext](https://example.org/e.png).\n\n- [x] done\n- [ ] todo\n\n> [!TIP] Tip & trick\n> be kind\n\n| Name | Qty |\n|:--|--:|\n| a&b | 1 |\n\n```mermaid\ngraph TD\n```\n\nFact[^1]\n\n[^1]: A note.\n\n## Details\n\n---\n\n# Second\n\nText";

fn book() -> Archive {
    let opts = EpubOptions {
        title: None,
        author: Some("Ada <Lovelace> & Co".into()),
        lang: Some("tr".into()),
        ..EpubOptions::default()
    };
    open(export_epub(BOOK, "book.md", &Provider, &opts).unwrap())
}

#[test]
fn epub_starts_with_a_stored_mimetype_entry() {
    let mut archive = book();
    let first = archive.by_index(0).unwrap();
    assert_eq!(first.name().unwrap(), "mimetype");
    assert_eq!(first.compression(), zip::CompressionMethod::Stored);
    assert_eq!(first.size(), first.compressed_size());
    drop(first);
    assert_eq!(
        read_bytes(&mut archive, "mimetype"),
        b"application/epub+zip"
    );
    // Everything else is present exactly once.
    let all = names(&mut archive);
    let mut unique = all.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(all.len(), unique.len(), "{all:?}");
    for wanted in [
        "META-INF/container.xml",
        "OEBPS/content.opf",
        "OEBPS/toc.ncx",
        "OEBPS/nav.xhtml",
        "OEBPS/stylesheet.css",
        "OEBPS/chapter_1.xhtml",
        "OEBPS/chapter_2.xhtml",
        "OEBPS/images/image-1.png",
    ] {
        assert!(all.iter().any(|n| n == wanted), "{wanted} in {all:?}");
    }
    assert_eq!(
        read_bytes(&mut archive, "OEBPS/images/image-1.png"),
        PNG_HEADER
    );
    assert_eq!(
        read_string(&mut archive, "OEBPS/stylesheet.css"),
        light_theme_css()
    );
}

#[test]
fn epub_container_points_at_the_package_document() {
    let mut archive = book();
    let container = parse_xml(&read_string(&mut archive, "META-INF/container.xml"));
    assert_eq!(container[0].name, "container");
    assert_eq!(container[0].attr("version"), Some("1.0"));
    let rootfile = container
        .iter()
        .find(|e| e.name == "rootfile")
        .expect("rootfile");
    assert_eq!(rootfile.attr("full-path"), Some("OEBPS/content.opf"));
    assert_eq!(
        rootfile.attr("media-type"),
        Some("application/oebps-package+xml")
    );
}

#[test]
fn epub_package_lists_every_file_and_spines_the_chapters() {
    let mut archive = book();
    let all = names(&mut archive);
    let opf = parse_xml(&read_string(&mut archive, "OEBPS/content.opf"));
    assert_eq!(opf[0].name, "package");
    assert_eq!(opf[0].attr("version"), Some("3.0"));
    let text_of = |name: &str| {
        opf.iter()
            .filter(|e| e.name == name)
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(text_of("title"), ["Field Notes"]);
    assert_eq!(text_of("language"), ["tr"]);
    assert_eq!(text_of("creator"), ["Ada <Lovelace> & Co"]);
    let identifier = opf.iter().find(|e| e.name == "identifier").unwrap();
    assert_eq!(opf[0].attr("unique-identifier"), identifier.attr("id"));
    assert!(
        identifier.text.starts_with("urn:uuid:"),
        "{}",
        identifier.text
    );

    let manifest: HashMap<String, (String, String)> = opf
        .iter()
        .filter(|e| e.name == "item")
        .map(|e| {
            (
                e.attr("id").unwrap().to_string(),
                (
                    e.attr("href").unwrap().to_string(),
                    e.attr("media-type").unwrap().to_string(),
                ),
            )
        })
        .collect();
    for (href, _) in manifest.values() {
        assert!(
            all.iter().any(|n| *n == format!("OEBPS/{href}")),
            "{href} missing from the zip"
        );
    }
    let media = |href: &str| {
        manifest
            .values()
            .find(|(h, _)| h == href)
            .map(|(_, m)| m.as_str())
    };
    assert_eq!(media("chapter_1.xhtml"), Some("application/xhtml+xml"));
    assert_eq!(media("stylesheet.css"), Some("text/css"));
    assert_eq!(media("images/image-1.png"), Some("image/png"));
    assert_eq!(media("toc.ncx"), Some("application/x-dtbncx+xml"));
    let nav = opf
        .iter()
        .find(|e| e.name == "item" && e.attr("href") == Some("nav.xhtml"))
        .unwrap();
    assert_eq!(nav.attr("properties"), Some("nav"));

    let spine: Vec<&str> = opf
        .iter()
        .filter(|e| e.name == "itemref")
        .map(|e| manifest[e.attr("idref").unwrap()].0.as_str())
        .collect();
    assert_eq!(spine, ["chapter_1.xhtml", "chapter_2.xhtml"]);
}

#[test]
fn epub_nav_lists_chapters_with_nested_headings() {
    let mut archive = book();
    let nav = parse_xml(&read_string(&mut archive, "OEBPS/nav.xhtml"));
    let navs: Vec<usize> = nav
        .iter()
        .enumerate()
        .filter(|(_, e)| e.name == "nav")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(navs.len(), 2, "toc and landmarks");
    let links: Vec<(&str, &str)> = nav[navs[0]..navs[1]]
        .iter()
        .filter(|e| e.name == "a")
        .map(|e| (e.attr("href").unwrap(), e.text.as_str()))
        .collect();
    assert_eq!(
        links,
        [
            ("chapter_1.xhtml", "Field Notes"),
            ("chapter_1.xhtml#details", "Details"),
            ("chapter_2.xhtml", "Second"),
        ]
    );
}

#[test]
fn epub_chapters_are_well_formed_xhtml_with_resolvable_links() {
    let mut archive = book();
    for chapter in ["OEBPS/chapter_1.xhtml", "OEBPS/chapter_2.xhtml"] {
        let xhtml = read_string(&mut archive, chapter);
        assert!(xhtml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n<html xmlns=\"http://www.w3.org/1999/xhtml\""), "{chapter}");
        let els = parse_xml(&xhtml);
        assert_eq!(els[0].name, "html");
        assert_eq!(els[0].attr("lang"), Some("tr"));
        let langs: Vec<&str> = els[0]
            .attrs
            .iter()
            .filter(|(k, _)| k == "lang")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(langs, ["tr", "tr"], "lang and xml:lang");
        for required in ["head", "title", "body"] {
            assert!(
                els.iter().any(|e| e.name == required),
                "{chapter}: <{required}>"
            );
        }
        // Identifiers are unique and every in-page link resolves.
        let ids: Vec<&str> = els.iter().filter_map(|e| e.attr("id")).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            ids.len(),
            unique.len(),
            "{chapter}: duplicate ids in {ids:?}"
        );
        for link in els
            .iter()
            .filter(|e| e.name == "a")
            .filter_map(|e| e.attr("href"))
        {
            if let Some(target) = link.strip_prefix('#') {
                assert!(ids.contains(&target), "{chapter}: dangling #{target}");
            }
        }
        // Boolean attributes are spelled out, void elements are self-closed (the parse above
        // would have rejected `<br>`), and no raw HTML5-only constructs remain.
        for input in els.iter().filter(|e| e.name == "input") {
            assert_eq!(input.attr("disabled"), Some("disabled"));
        }
        assert!(
            !xhtml.contains("<br>") && !xhtml.contains("<hr>") && !xhtml.contains(" disabled>"),
            "{chapter}"
        );
    }
}

#[test]
fn epub_chapter_content_follows_the_markdown() {
    let mut archive = book();
    let one = read_string(&mut archive, "OEBPS/chapter_1.xhtml");
    let els = parse_xml(&one);
    let text_of = |name: &str| {
        els.iter()
            .filter(|e| e.name == name)
            .map(|e| e.text.trim().to_string())
            .collect::<Vec<_>>()
    };
    // The first <title> is the page title (the math SVG carries another one).
    assert_eq!(text_of("title")[0], "Field Notes");
    assert_eq!(text_of("h1"), ["Field Notes"]);
    assert_eq!(text_of("h2"), ["Details"]);
    let imgs: Vec<(&str, &str)> = els
        .iter()
        .filter(|e| e.name == "img")
        .map(|e| (e.attr("src").unwrap(), e.attr("alt").unwrap()))
        .collect();
    assert_eq!(
        imgs,
        [
            ("images/image-1.png", "logo"),
            ("https://example.org/e.png", "ext")
        ]
    );
    assert!(one.contains("<br />\n"));
    assert!(one.contains("<span class=\"math inline\"><svg xmlns=\"http://www.w3.org/2000/svg\" data-display=\"false\"><title>x^2</title></svg></span>"));
    assert!(one.contains("<div class=\"mermaid-diagram\"><svg"));
    assert!(one.contains("<p class=\"callout-title\">Tip &amp; trick</p>"));
    assert!(one.contains(
        "<td style=\"text-align:left\">a&amp;b</td><td style=\"text-align:right\">1</td>"
    ));
    assert!(one.contains("<li class=\"task-list-item done\"><input type=\"checkbox\" disabled=\"disabled\" checked=\"checked\" /> done</li>"));
    assert!(one.contains("<section class=\"footnotes\">\n<hr />\n<ol>\n<li id=\"fn-1\">"));
    assert!(
        one.contains("<hr />\n<section"),
        "thematic break before the footnotes: {one}"
    );
    let two = read_string(&mut archive, "OEBPS/chapter_2.xhtml");
    let els = parse_xml(&two);
    assert!(els.iter().any(|e| e.name == "h1" && e.text == "Second"));
    assert!(els.iter().any(|e| e.name == "title" && e.text == "Second"));
    assert!(!two.contains("Field Notes") && !two.contains("footnotes"));
}

#[test]
fn epub_without_headings_is_a_single_chapter_titled_after_the_file() {
    let mut archive = open(
        export_epub(
            "just text & more",
            "C:\\docs\\my notes.md",
            &NoAssets,
            &EpubOptions::default(),
        )
        .unwrap(),
    );
    let all = names(&mut archive);
    assert_eq!(
        all.iter()
            .filter(|n| n.starts_with("OEBPS/chapter_"))
            .count(),
        1,
        "{all:?}"
    );
    let opf = parse_xml(&read_string(&mut archive, "OEBPS/content.opf"));
    assert_eq!(
        opf.iter().find(|e| e.name == "title").unwrap().text,
        "my notes"
    );
    assert_eq!(
        opf.iter().find(|e| e.name == "language").unwrap().text,
        "en"
    );
    assert!(opf.iter().all(|e| e.name != "creator"));
    let els = parse_xml(&read_string(&mut archive, "OEBPS/chapter_1.xhtml"));
    assert!(
        els.iter()
            .any(|e| e.name == "p" && e.text == "just text & more")
    );
}

// ---------------------------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------------------------

#[test]
fn copy_as_plain_text_strips_markers_and_keeps_structure() {
    let md = "# Title\n\nSome **bold**, *italic*, ~~gone~~, ==marked== and `code` with a [link](https://example.org) and ![pic](a.png).\n\n- one\n- two\n  1. nested\n\nDone list:\n\n- [x] done\n- [ ] todo\n\n> [!NOTE]\n> remember\n\n| A | B |\n|---|--:|\n| 1 | 22 |\n\n```rust\nlet x = 1;\n```\n\nFoot[^1]\n\n[^1]: the note\n";
    assert_eq!(
        markdown_to_text(md),
        "Title\n\nSome bold, italic, gone, marked and code with a link and pic.\n\n- one\n- two\n  1. nested\n\nDone list:\n\n- [x] done\n- [ ] todo\n\nNOTE\n  remember\n\nA   B\n-  --\n1  22\n\nlet x = 1;\n\nFoot[1]\n\n[1] the note\n"
    );
}

#[test]
fn plain_text_unescapes_and_decodes_what_markdown_escaped() {
    assert_eq!(
        markdown_to_text("a\\*b \\_c\\_ &lt;tag&gt; &amp; \\# not a heading"),
        "a*b _c_ <tag> & # not a heading\n"
    );
    assert_eq!(
        markdown_to_text("日本語 **強調** çağrı"),
        "日本語 強調 çağrı\n"
    );
    assert_eq!(markdown_to_text(""), "");
}
