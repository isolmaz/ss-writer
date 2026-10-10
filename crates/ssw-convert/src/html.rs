//! Markdown → HTML5 (single-file export and clipboard fragment) and the shared renderer used by
//! the ePub exporter.
//!
//! Raw HTML in the Markdown is never interpreted: like in the editor it is shown as text
//! (escaped), so an exported file cannot carry scripts from a pasted snippet. `javascript:`,
//! `vbscript:` and `data:` link destinations are neutralized for the same reason.

use std::collections::HashMap;

use ssw_core::markdown::ast::{
    Alignment, Block, BlockKind, CalloutKind, Image, Inline, InlineKind, Table, TableRow,
};
use ssw_core::markdown::definitions::normalize_label;
use ssw_core::markdown::{parse_document, unescape_markdown};

use crate::assets::{AssetProvider, data_uri, extension_for_mime, is_external_url};
use crate::css::{self, highlight_hex};
use crate::txt::inline_plain;

/// Options of a standalone HTML document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlOptions {
    /// `<title>` text; defaults to the text of the first level-1 heading.
    pub title: Option<String>,
    /// Value of `<html lang>`; omitted when `None`.
    pub lang: Option<String>,
}

/// Standalone HTML5 document: embedded light-theme CSS, images as base64 data URIs, math and
/// Mermaid as inline SVG.
pub fn render_document(src: &str, provider: &dyn AssetProvider, opts: &HtmlOptions) -> String {
    let doc = parse_document(src);
    let mut r = Renderer::new(
        &doc.blocks,
        src,
        provider,
        Flavor::Html5,
        ImageMode::DataUri,
    );
    let mut body = String::new();
    r.blocks(&doc.blocks, &mut body);
    r.footnotes_section(&mut body);
    let title = match &opts.title {
        Some(t) => t.clone(),
        None => r
            .headings
            .iter()
            .find(|h| h.level == 1)
            .map(|h| h.text.clone())
            .unwrap_or_default(),
    };
    let mut out = String::with_capacity(body.len() + 4096);
    out.push_str("<!DOCTYPE html>\n<html");
    if let Some(lang) = &opts.lang {
        out.push_str(" lang=\"");
        push_escaped(&mut out, lang, true);
        out.push('"');
    }
    out.push_str(">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>",
    );
    push_escaped(&mut out, &title, false);
    out.push_str("</title>\n<style>\n");
    out.push_str(css::light_theme_css());
    out.push_str("</style>\n</head>\n<body>\n<main>\n");
    out.push_str(&body);
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

/// HTML fragment for the clipboard: no document wrapper or style sheet; the few styles that
/// matter to receiving applications (code font, highlight color, table borders) are inline.
pub fn render_fragment(src: &str, provider: &dyn AssetProvider) -> String {
    let doc = parse_document(src);
    let mut r = Renderer::new(
        &doc.blocks,
        src,
        provider,
        Flavor::Html5,
        ImageMode::DataUri,
    );
    r.inline_styles = true;
    let mut out = String::new();
    r.blocks(&doc.blocks, &mut out);
    r.footnotes_section(&mut out);
    out
}

/// Escapes `&`, `<` and `>` for element content.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_escaped(&mut out, s, false);
    out
}

/// Escapes `&`, `<`, `>` and `"` for a double-quoted attribute value.
pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    push_escaped(&mut out, s, true);
    out
}

pub(crate) fn push_escaped(out: &mut String, s: &str, attr: bool) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

/// Anchor id for a heading: lower-cased alphanumerics joined by `-`.
pub fn heading_slug(text: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.extend(c.to_lowercase());
        } else if c.is_whitespace() || c == '-' || c == '_' {
            pending_dash = true;
        }
    }
    if out.is_empty() {
        out.push_str("section");
    }
    out
}

/// Percent-encodes everything but unreserved characters and `/`.
pub(crate) fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Link destination that cannot execute script; unsafe schemes collapse to `#`.
fn safe_href(dest: &str) -> String {
    let probe: String = dest
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .collect::<String>()
        .to_ascii_lowercase();
    if ["javascript:", "vbscript:", "data:"]
        .iter()
        .any(|p| probe.starts_with(p))
    {
        "#".to_string()
    } else {
        dest.to_string()
    }
}

/// Output syntax of the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Html5,
    /// Well-formed XML (void elements self-closed, boolean attributes spelled out) for ePub.
    Xhtml,
}

/// Where image bytes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageMode {
    /// Base64 `data:` URIs inside the markup.
    DataUri,
    /// Collected in [`Renderer::resources`] and referenced as `images/image-N.ext`.
    Resources,
}

/// An image collected in [`ImageMode::Resources`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageResource {
    /// Path relative to the HTML file.
    pub path: String,
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

/// A heading seen while rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadingInfo {
    pub level: u8,
    pub id: String,
    pub text: String,
}

pub(crate) struct Renderer<'a> {
    src: &'a str,
    provider: &'a dyn AssetProvider,
    flavor: Flavor,
    images: ImageMode,
    inline_styles: bool,
    footnote_defs: HashMap<String, &'a Block>,
    used_footnotes: Vec<String>,
    image_paths: HashMap<String, String>,
    slugs: HashMap<String, usize>,
    pub(crate) resources: Vec<ImageResource>,
    pub(crate) headings: Vec<HeadingInfo>,
}

impl<'a> Renderer<'a> {
    pub(crate) fn new(
        all_blocks: &'a [Block],
        src: &'a str,
        provider: &'a dyn AssetProvider,
        flavor: Flavor,
        images: ImageMode,
    ) -> Self {
        let mut footnote_defs = HashMap::new();
        let mut stack: Vec<&'a Block> = all_blocks.iter().rev().collect();
        while let Some(b) = stack.pop() {
            if let BlockKind::FootnoteDefinition { label } = &b.kind {
                footnote_defs.entry(normalize_label(label)).or_insert(b);
            }
            stack.extend(b.children.iter().rev());
        }
        Renderer {
            src,
            provider,
            flavor,
            images,
            inline_styles: false,
            footnote_defs,
            used_footnotes: Vec::new(),
            image_paths: HashMap::new(),
            slugs: HashMap::new(),
            resources: Vec::new(),
            headings: Vec::new(),
        }
    }

    fn vend(&self) -> &'static str {
        match self.flavor {
            Flavor::Html5 => ">",
            Flavor::Xhtml => " />",
        }
    }

    fn text_of(&self, range: ssw_core::text::TextRange) -> &'a str {
        range.get(self.src).unwrap_or("")
    }

    pub(crate) fn blocks(&mut self, blocks: &'a [Block], out: &mut String) {
        for b in blocks {
            self.block(b, out);
        }
    }

    fn block(&mut self, b: &'a Block, out: &mut String) {
        match &b.kind {
            BlockKind::Heading { level, .. } => self.heading(b, *level, out),
            BlockKind::Paragraph => {
                out.push_str("<p>");
                self.inlines(&b.inlines, out);
                out.push_str("</p>\n");
            }
            BlockKind::List {
                ordered,
                start,
                tight,
                ..
            } => {
                let tag = if *ordered { "ol" } else { "ul" };
                out.push('<');
                out.push_str(tag);
                if *ordered && *start != 1 {
                    out.push_str(&format!(" start=\"{start}\""));
                }
                out.push_str(">\n");
                for item in &b.children {
                    self.list_item(item, *tight, out);
                }
                out.push_str(&format!("</{tag}>\n"));
            }
            BlockKind::ListItem { .. } => self.list_item(b, true, out),
            BlockKind::BlockQuote => {
                out.push_str("<blockquote>\n");
                self.blocks(&b.children, out);
                out.push_str("</blockquote>\n");
            }
            BlockKind::Callout { kind, title, .. } => {
                let kw = kind.keyword().to_ascii_lowercase();
                out.push_str(&format!(
                    "<div class=\"callout callout-{kw}\">\n<p class=\"callout-title\">"
                ));
                match title {
                    Some(t) => push_escaped(out, self.text_of(*t), false),
                    None => push_escaped(out, &title_case(*kind), false),
                }
                out.push_str("</p>\n");
                self.blocks(&b.children, out);
                out.push_str("</div>\n");
            }
            BlockKind::CodeBlock(cb) => {
                let code = cb.text(self.src);
                let lang = cb.language(self.src).map(unescape_markdown);
                if lang.as_deref() == Some("mermaid")
                    && let Some(svg) = self.provider.mermaid_svg(code.trim_end_matches('\n'))
                {
                    out.push_str("<div class=\"mermaid-diagram\">");
                    out.push_str(&svg);
                    out.push_str("</div>\n");
                    return;
                }
                self.code_block(&code, lang.as_deref(), out);
            }
            BlockKind::MathBlock(m) => {
                let tex = m.text(self.src);
                match self.provider.math_svg(&tex, true) {
                    Some(svg) => {
                        out.push_str("<div class=\"math display\">");
                        out.push_str(&svg);
                        out.push_str("</div>\n");
                    }
                    None => {
                        out.push_str("<pre class=\"math\"><code>");
                        push_escaped(out, &tex, false);
                        out.push_str("</code></pre>\n");
                    }
                }
            }
            BlockKind::Table(t) => self.table(t, out),
            BlockKind::ThematicBreak => {
                out.push_str("<hr");
                out.push_str(self.vend());
                out.push('\n');
            }
            BlockKind::HtmlBlock(h) => {
                if b.inlines.is_empty() {
                    out.push_str("<pre class=\"raw-html\">");
                    push_escaped(out, h.text(self.src).trim_end_matches('\n'), false);
                    out.push_str("</pre>\n");
                } else {
                    out.push_str("<p>");
                    self.inlines(&b.inlines, out);
                    out.push_str("</p>\n");
                }
            }
            BlockKind::PageBreak => out.push_str("<div class=\"page-break\"></div>\n"),
            BlockKind::FrontMatter { .. }
            | BlockKind::FootnoteDefinition { .. }
            | BlockKind::LinkReferenceDefinition(_) => {}
        }
    }

    fn heading(&mut self, b: &'a Block, level: u8, out: &mut String) {
        let mut text = String::new();
        inline_plain(&b.inlines, self.src, " ", false, &mut text);
        let base = heading_slug(&text);
        let n = self.slugs.entry(base.clone()).or_insert(0);
        let id = if *n == 0 { base } else { format!("{base}-{n}") };
        *n += 1;
        out.push_str(&format!("<h{level} id=\""));
        push_escaped(out, &id, true);
        out.push_str("\">");
        self.inlines(&b.inlines, out);
        out.push_str(&format!("</h{level}>\n"));
        self.headings.push(HeadingInfo { level, id, text });
    }

    fn code_block(&self, code: &str, lang: Option<&str>, out: &mut String) {
        out.push_str(if self.inline_styles {
            "<pre style=\"font-family:'Cascadia Mono',Consolas,monospace\"><code"
        } else {
            "<pre><code"
        });
        if let Some(l) = lang {
            out.push_str(" class=\"language-");
            push_escaped(out, l, true);
            out.push('"');
        }
        out.push('>');
        push_escaped(out, code, false);
        out.push_str("</code></pre>\n");
    }

    fn list_item(&mut self, item: &'a Block, tight: bool, out: &mut String) {
        if let BlockKind::ListItem { task: Some(t), .. } = &item.kind {
            out.push_str("<li class=\"task-list-item");
            if t.checked {
                out.push_str(" done");
            }
            out.push_str("\"><input type=\"checkbox\"");
            match (self.flavor, t.checked) {
                (Flavor::Html5, false) => out.push_str(" disabled"),
                (Flavor::Html5, true) => out.push_str(" disabled checked"),
                (Flavor::Xhtml, false) => out.push_str(" disabled=\"disabled\""),
                (Flavor::Xhtml, true) => out.push_str(" disabled=\"disabled\" checked=\"checked\""),
            }
            out.push_str(self.vend());
            out.push(' ');
        } else {
            out.push_str("<li>");
        }
        let count = item.children.len();
        for (i, child) in item.children.iter().enumerate() {
            if tight && child.kind == BlockKind::Paragraph {
                self.inlines(&child.inlines, out);
                if i + 1 < count {
                    out.push('\n');
                }
            } else {
                if i == 0 && !matches!(child.kind, BlockKind::Paragraph) {
                    out.push('\n');
                }
                self.block(child, out);
            }
        }
        out.push_str("</li>\n");
    }

    fn table(&mut self, t: &'a Table, out: &mut String) {
        if self.inline_styles {
            out.push_str("<table border=\"1\" cellspacing=\"0\" cellpadding=\"4\">\n");
        } else {
            out.push_str("<table>\n");
        }
        out.push_str("<thead>\n");
        self.table_row(&t.header, "th", &t.alignments, out);
        out.push_str("</thead>\n");
        if !t.rows.is_empty() {
            out.push_str("<tbody>\n");
            for row in &t.rows {
                self.table_row(row, "td", &t.alignments, out);
            }
            out.push_str("</tbody>\n");
        }
        out.push_str("</table>\n");
    }

    fn table_row(&mut self, row: &'a TableRow, tag: &str, aligns: &[Alignment], out: &mut String) {
        out.push_str("<tr>");
        for (i, cell) in row.cells.iter().enumerate() {
            out.push('<');
            out.push_str(tag);
            let style = match aligns.get(i) {
                Some(Alignment::Left) => " style=\"text-align:left\"",
                Some(Alignment::Center) => " style=\"text-align:center\"",
                Some(Alignment::Right) => " style=\"text-align:right\"",
                Some(Alignment::None) | None => "",
            };
            out.push_str(style);
            out.push('>');
            self.inlines(&cell.inlines, out);
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
        out.push_str("</tr>\n");
    }

    pub(crate) fn inlines(&mut self, items: &'a [Inline], out: &mut String) {
        for n in items {
            self.inline(n, out);
        }
    }

    fn wrap(&mut self, tag: &str, n: &'a Inline, out: &mut String) {
        out.push('<');
        out.push_str(tag);
        out.push('>');
        self.inlines(&n.children, out);
        out.push_str("</");
        out.push_str(tag);
        out.push('>');
    }

    fn inline(&mut self, n: &'a Inline, out: &mut String) {
        match &n.kind {
            InlineKind::Text => push_escaped(out, self.text_of(n.range), false),
            InlineKind::Escape => push_escaped(out, self.text_of(n.content), false),
            InlineKind::Entity { decoded } => push_escaped(out, decoded, false),
            InlineKind::Emphasis => self.wrap("em", n, out),
            InlineKind::Strong => self.wrap("strong", n, out),
            InlineKind::Underline => self.wrap("u", n, out),
            InlineKind::Strikethrough => self.wrap("del", n, out),
            InlineKind::Highlight { color } => {
                if self.inline_styles {
                    out.push_str(&format!(
                        "<mark style=\"background-color:{}\">",
                        highlight_hex(*color)
                    ));
                } else if let Some(c) = color {
                    out.push_str(&format!("<mark class=\"hl-{}\">", c.name()));
                } else {
                    out.push_str("<mark>");
                }
                self.inlines(&n.children, out);
                out.push_str("</mark>");
            }
            InlineKind::Code { text } => {
                out.push_str(if self.inline_styles {
                    "<code style=\"font-family:'Cascadia Mono',Consolas,monospace\">"
                } else {
                    "<code>"
                });
                push_escaped(out, text, false);
                out.push_str("</code>");
            }
            InlineKind::Link(l) => {
                out.push_str("<a href=\"");
                push_escaped(out, &safe_href(&l.dest), true);
                out.push('"');
                if let Some(t) = &l.title {
                    out.push_str(" title=\"");
                    push_escaped(out, t, true);
                    out.push('"');
                }
                out.push('>');
                self.inlines(&n.children, out);
                out.push_str("</a>");
            }
            InlineKind::Image(img) => self.image(n, img, out),
            InlineKind::WikiLink(w) => {
                let mut href = String::new();
                if !w.target.is_empty() {
                    href.push_str(&percent_encode_path(&w.target));
                    href.push_str(".html");
                }
                if let Some(h) = &w.heading {
                    href.push('#');
                    href.push_str(&heading_slug(h));
                }
                out.push_str("<a class=\"wikilink\" href=\"");
                push_escaped(out, &href, true);
                out.push_str("\">");
                // `[[#Heading]]` reads as the heading text, without the `#`.
                let shown = match (&w.alias, &w.heading) {
                    (None, Some(heading)) if w.target.is_empty() => heading.as_str(),
                    _ => self.text_of(n.content),
                };
                push_escaped(out, shown, false);
                out.push_str("</a>");
            }
            InlineKind::FootnoteRef { label } => {
                let key = normalize_label(label);
                let number = match self.used_footnotes.iter().position(|k| *k == key) {
                    Some(p) => p + 1,
                    None => {
                        self.used_footnotes.push(key);
                        self.used_footnotes.len()
                    }
                };
                out.push_str(&format!(
                    "<sup class=\"footnote-ref\"><a href=\"#fn-{number}\" id=\"fnref-{number}\">{number}</a></sup>"
                ));
            }
            InlineKind::InlineMath => {
                let tex = self.text_of(n.content);
                match self.provider.math_svg(tex, false) {
                    Some(svg) => {
                        out.push_str("<span class=\"math inline\">");
                        out.push_str(&svg);
                        out.push_str("</span>");
                    }
                    None => {
                        out.push_str("<code class=\"math\">");
                        push_escaped(out, tex, false);
                        out.push_str("</code>");
                    }
                }
            }
            InlineKind::HardBreak => {
                out.push_str("<br");
                out.push_str(self.vend());
                out.push('\n');
            }
            InlineKind::SoftBreak => out.push('\n'),
            InlineKind::RawHtml => push_escaped(out, self.text_of(n.range), false),
        }
    }

    fn image(&mut self, n: &'a Inline, img: &'a Image, out: &mut String) {
        let alt = if n.children.is_empty() {
            unescape_markdown(self.text_of(img.alt))
        } else {
            let mut s = String::new();
            inline_plain(&n.children, self.src, " ", false, &mut s);
            s
        };
        let src = self.image_src(&img.dest);
        out.push_str("<img src=\"");
        push_escaped(out, &src, true);
        out.push_str("\" alt=\"");
        push_escaped(out, &alt, true);
        out.push('"');
        if let Some(t) = &img.title {
            out.push_str(" title=\"");
            push_escaped(out, t, true);
            out.push('"');
        }
        if let Some(w) = img.width {
            out.push_str(&format!(" width=\"{w}\""));
        }
        if let Some(h) = img.height {
            out.push_str(&format!(" height=\"{h}\""));
        }
        out.push_str(self.vend());
    }

    fn image_src(&mut self, dest: &str) -> String {
        if is_external_url(dest) {
            return dest.to_string();
        }
        let Some((bytes, mime)) = self.provider.image(dest) else {
            return dest.to_string();
        };
        match self.images {
            ImageMode::DataUri => data_uri(&bytes, mime),
            ImageMode::Resources => {
                if let Some(path) = self.image_paths.get(dest) {
                    return path.clone();
                }
                let path = format!(
                    "images/image-{}.{}",
                    self.resources.len() + 1,
                    extension_for_mime(mime)
                );
                self.resources.push(ImageResource {
                    path: path.clone(),
                    bytes,
                    mime,
                });
                self.image_paths.insert(dest.to_string(), path.clone());
                path
            }
        }
    }

    /// Appends the footnotes referenced so far as an ordered list and starts a fresh numbering.
    pub(crate) fn footnotes_section(&mut self, out: &mut String) {
        if self.used_footnotes.is_empty() {
            return;
        }
        out.push_str("<section class=\"footnotes\">\n<hr");
        out.push_str(self.vend());
        out.push_str("\n<ol>\n");
        let mut i = 0;
        // Footnote bodies may reference further footnotes, which extends the list.
        while i < self.used_footnotes.len() {
            let number = i + 1;
            let def = self.footnote_defs.get(&self.used_footnotes[i]).copied();
            let mut body = String::new();
            if let Some(def) = def {
                self.blocks(&def.children, &mut body);
            }
            let back =
                format!(" <a href=\"#fnref-{number}\" class=\"footnote-backref\">\u{21A9}</a>");
            match body.rfind("</p>") {
                Some(pos) => body.insert_str(pos, &back),
                None => body.push_str(&format!("<p>{}</p>\n", back.trim_start())),
            }
            out.push_str(&format!("<li id=\"fn-{number}\">\n{body}</li>\n"));
            i += 1;
        }
        out.push_str("</ol>\n</section>\n");
        self.used_footnotes.clear();
    }
}

fn title_case(kind: CalloutKind) -> String {
    // Keywords are non-empty ASCII (`NOTE`, `TIP`, …).
    let kw = kind.keyword();
    format!("{}{}", &kw[..1], kw[1..].to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::NoAssets;

    struct Fake;
    impl AssetProvider for Fake {
        fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
            (path == "pic.png").then(|| (vec![1, 2, 3], "image/png"))
        }
        fn math_svg(&self, latex: &str, display: bool) -> Option<String> {
            Some(format!(
                "<svg data-d=\"{display}\"><title>{latex}</title></svg>"
            ))
        }
        fn mermaid_svg(&self, source: &str) -> Option<String> {
            Some(format!("<svg><desc>{}</desc></svg>", source.trim()))
        }
    }

    fn frag(md: &str) -> String {
        render_fragment(md, &NoAssets)
    }

    fn body(md: &str) -> String {
        let doc = parse_document(md);
        let mut r = Renderer::new(
            &doc.blocks,
            md,
            &NoAssets,
            Flavor::Html5,
            ImageMode::DataUri,
        );
        let mut out = String::new();
        r.blocks(&doc.blocks, &mut out);
        r.footnotes_section(&mut out);
        out
    }

    #[test]
    fn unsafe_link_schemes_are_neutralized() {
        let table = [
            ("JaVaScript:alert(1)", "#"),
            (" java\tscript:x", "#"),
            ("vbscript:x", "#"),
            ("data:text/html,x", "#"),
            ("https://ok", "https://ok"),
            ("rel/path.md", "rel/path.md"),
        ];
        for (input, expected) in table {
            assert_eq!(safe_href(input), expected, "{input:?}");
        }
    }

    #[test]
    fn escaping_text_and_attributes() {
        assert_eq!(escape_text("a<b>&\"c\""), "a&lt;b&gt;&amp;\"c\"");
        assert_eq!(escape_attr("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn slugs_follow_alphanumerics() {
        let table = [
            ("Hello, World!", "hello-world"),
            ("  Spaced   out ", "spaced-out"),
            ("a-b_c", "a-b-c"),
            ("İstanbul Çay", "i\u{307}stanbul-çay"),
            ("???", "section"),
        ];
        for (input, expected) in table {
            assert_eq!(heading_slug(input), expected, "{input}");
        }
    }

    #[test]
    fn blocks_table() {
        let table: &[(&str, &str)] = &[
            ("# Title", "<h1 id=\"title\">Title</h1>\n"),
            (
                "Para *em* **st** ~u~ ~~s~~",
                "<p>Para <em>em</em> <strong>st</strong> <u>u</u> <del>s</del></p>\n",
            ),
            ("a\\*b &amp; c", "<p>a*b &amp; c</p>\n"),
            ("`x<y`", "<p><code>x&lt;y</code></p>\n"),
            (
                "==hi== =={red}r==",
                "<p><mark>hi</mark> <mark class=\"hl-red\">r</mark></p>\n",
            ),
            ("> quote", "<blockquote>\n<p>quote</p>\n</blockquote>\n"),
            ("---", "<hr>\n"),
            ("a  \nb\nc", "<p>a<br>\nb\nc</p>\n"),
            ("<!-- pagebreak -->", "<div class=\"page-break\"></div>\n"),
            (
                "```rust\nfn f() {}\n```",
                "<pre><code class=\"language-rust\">fn f() {}\n</code></pre>\n",
            ),
            ("```\nplain\n```", "<pre><code>plain\n</code></pre>\n"),
            (
                "$$\nx^2\n$$",
                "<pre class=\"math\"><code>x^2</code></pre>\n",
            ),
            (
                "so $a<b$ ok",
                "<p>so <code class=\"math\">a&lt;b</code> ok</p>\n",
            ),
            (
                "<div>raw</div>",
                "<pre class=\"raw-html\">&lt;div&gt;raw&lt;/div&gt;</pre>\n",
            ),
            ("x <b>y</b>", "<p>x &lt;b&gt;y&lt;/b&gt;</p>\n"),
            (
                "[a](http://x \"T\")",
                "<p><a href=\"http://x\" title=\"T\">a</a></p>\n",
            ),
            ("[a](javascript:alert(1))", "<p><a href=\"#\">a</a></p>\n"),
            (
                "[[My Note]] [[#Intro Part]] [[a#B|alias]]",
                "<p><a class=\"wikilink\" href=\"My%20Note.html\">My Note</a> <a class=\"wikilink\" href=\"#intro-part\">Intro Part</a> <a class=\"wikilink\" href=\"a.html#b\">alias</a></p>\n",
            ),
            (
                "![alt *x*|300x200](pic.png \"t\")",
                "<p><img src=\"pic.png\" alt=\"alt x\" title=\"t\" width=\"300\" height=\"200\"></p>\n",
            ),
            (
                "![a](https://h/x.png)",
                "<p><img src=\"https://h/x.png\" alt=\"a\"></p>\n",
            ),
            ("---\ntitle: x\n---\nbody", "<p>body</p>\n"),
            (
                "[x]: http://e.com\n\n[x]",
                "<p><a href=\"http://e.com\">x</a></p>\n",
            ),
        ];
        for (md, expected) in table {
            assert_eq!(body(md), *expected, "{md:?}");
        }
    }

    #[test]
    fn lists_and_tasks() {
        assert_eq!(body("- a\n- b"), "<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n");
        assert_eq!(
            body("3. a\n4. b"),
            "<ol start=\"3\">\n<li>a</li>\n<li>b</li>\n</ol>\n"
        );
        assert_eq!(body("1. a"), "<ol>\n<li>a</li>\n</ol>\n");
        assert_eq!(
            body("- a\n  - b"),
            "<ul>\n<li>a\n<ul>\n<li>b</li>\n</ul>\n</li>\n</ul>\n"
        );
        assert_eq!(
            body("- a\n\n- b"),
            "<ul>\n<li><p>a</p>\n</li>\n<li><p>b</p>\n</li>\n</ul>\n"
        );
        assert_eq!(
            body("- [ ] open\n- [x] done"),
            "<ul>\n<li class=\"task-list-item\"><input type=\"checkbox\" disabled> open</li>\n<li class=\"task-list-item done\"><input type=\"checkbox\" disabled checked> done</li>\n</ul>\n"
        );
        assert_eq!(
            body("- ```\n  c\n  ```"),
            "<ul>\n<li>\n<pre><code>c\n</code></pre>\n</li>\n</ul>\n"
        );
    }

    #[test]
    fn a_list_item_rendered_on_its_own_is_a_tight_item() {
        let md = "- [x] a\n- b";
        let doc = parse_document(md);
        let mut r = Renderer::new(
            &doc.blocks,
            md,
            &NoAssets,
            Flavor::Html5,
            ImageMode::DataUri,
        );
        let mut out = String::new();
        r.blocks(&doc.blocks[0].children, &mut out);
        assert_eq!(
            out,
            "<li class=\"task-list-item done\"><input type=\"checkbox\" disabled checked> a</li>\n<li>b</li>\n"
        );
    }

    #[test]
    fn a_lone_img_tag_block_becomes_a_paragraph_with_the_image() {
        let md = "<img src=\"pic.png\" width=\"40\" height=\"30\" alt=\"x\">\n";
        let out = render_document(md, &Fake, &HtmlOptions::default());
        assert!(out.contains("<main>\n<p><img src=\"data:image/png;base64,AQID\" alt=\"x\" width=\"40\" height=\"30\"></p>\n</main>"), "{out}");
    }

    #[test]
    fn tables_with_alignment() {
        let md = "| a | b | c | d |\n|:--|:-:|--:|---|\n| 1 | 2 | 3 | 4 |";
        assert_eq!(
            body(md),
            "<table>\n<thead>\n<tr><th style=\"text-align:left\">a</th><th style=\"text-align:center\">b</th><th style=\"text-align:right\">c</th><th>d</th></tr>\n</thead>\n<tbody>\n<tr><td style=\"text-align:left\">1</td><td style=\"text-align:center\">2</td><td style=\"text-align:right\">3</td><td>4</td></tr>\n</tbody>\n</table>\n"
        );
        let header_only = "| a |\n|---|";
        assert_eq!(
            body(header_only),
            "<table>\n<thead>\n<tr><th>a</th></tr>\n</thead>\n</table>\n"
        );
        let ragged = "| a |\n|---|\n| 1 | 2 |";
        assert!(body(ragged).contains("<td>1</td><td>2</td>"));
    }

    #[test]
    fn callouts_default_and_custom_title() {
        let out = body("> [!WARNING]\n> careful");
        assert_eq!(
            out,
            "<div class=\"callout callout-warning\">\n<p class=\"callout-title\">Warning</p>\n<p>careful</p>\n</div>\n"
        );
        let out = body("> [!tip] My title\n> body");
        assert!(out.contains("callout-tip"));
        assert!(out.contains("<p class=\"callout-title\">My title</p>"));
    }

    #[test]
    fn footnotes_are_numbered_by_first_reference_and_linked_back() {
        let md = "a[^b] and c[^a] and again[^b]\n\n[^a]: first *def*\n\n[^b]: second";
        let out = body(md);
        assert!(out.starts_with("<p>a<sup class=\"footnote-ref\"><a href=\"#fn-1\" id=\"fnref-1\">1</a></sup> and c<sup class=\"footnote-ref\"><a href=\"#fn-2\" id=\"fnref-2\">2</a></sup> and again<sup class=\"footnote-ref\"><a href=\"#fn-1\" id=\"fnref-1\">1</a></sup></p>\n"));
        assert!(out.contains("<li id=\"fn-1\">\n<p>second <a href=\"#fnref-1\" class=\"footnote-backref\">\u{21A9}</a></p>\n</li>"));
        assert!(out.contains("<li id=\"fn-2\">\n<p>first <em>def</em> <a href=\"#fnref-2\" class=\"footnote-backref\">\u{21A9}</a></p>\n</li>"));
        assert!(out.contains("<section class=\"footnotes\">"));
    }

    #[test]
    fn footnote_without_paragraph_body_gets_backlink_paragraph() {
        let out = body("x[^1]\n\n[^1]:");
        assert!(out.contains("<li id=\"fn-1\">\n<p><a href=\"#fnref-1\" class=\"footnote-backref\">\u{21A9}</a></p>\n</li>"), "{out}");
    }

    #[test]
    fn footnotes_inside_footnotes_extend_the_list() {
        let out = body("x[^1]\n\n[^1]: see[^2]\n[^2]: inner");
        assert!(out.contains("<li id=\"fn-2\">"), "{out}");
    }

    #[test]
    fn document_has_embedded_css_title_and_headings() {
        let doc = render_document("# My *Title*\n\ntext", &NoAssets, &HtmlOptions::default());
        assert!(doc.starts_with("<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">"));
        assert!(doc.contains("<title>My Title</title>"));
        assert!(doc.contains("<style>\n:root{color-scheme:light}"));
        assert!(
            doc.contains(
                "<main>\n<h1 id=\"my-title\">My <em>Title</em></h1>\n<p>text</p>\n</main>"
            )
        );
        assert!(doc.ends_with("</html>\n"));
    }

    #[test]
    fn document_options_override_title_and_set_lang() {
        let opts = HtmlOptions {
            title: Some("A & B".into()),
            lang: Some("tr\"".into()),
        };
        let doc = render_document("# Other", &NoAssets, &opts);
        assert!(doc.contains("<html lang=\"tr&quot;\">"));
        assert!(doc.contains("<title>A &amp; B</title>"));
        let none = render_document("no heading", &NoAssets, &HtmlOptions::default());
        assert!(none.contains("<title></title>"));
    }

    #[test]
    fn duplicate_headings_get_unique_ids() {
        let out = body("# A\n\n# A\n\n# A");
        assert!(out.contains("id=\"a\""));
        assert!(out.contains("id=\"a-1\""));
        assert!(out.contains("id=\"a-2\""));
    }

    #[test]
    fn provider_assets_are_embedded() {
        let md = "![p](pic.png)\n\n![missing](nope.png)\n\n$x$ and\n\n$$\ny\n$$\n\n```mermaid\ngraph TD\n```";
        let out = render_document(md, &Fake, &HtmlOptions::default());
        assert!(out.contains("<img src=\"data:image/png;base64,AQID\" alt=\"p\">"));
        assert!(out.contains("<img src=\"nope.png\" alt=\"missing\">"));
        assert!(out.contains(
            "<span class=\"math inline\"><svg data-d=\"false\"><title>x</title></svg></span>"
        ));
        assert!(out.contains(
            "<div class=\"math display\"><svg data-d=\"true\"><title>y</title></svg></div>"
        ));
        assert!(
            out.contains("<div class=\"mermaid-diagram\"><svg><desc>graph TD</desc></svg></div>")
        );
    }

    #[test]
    fn mermaid_without_renderer_is_a_code_block() {
        let out = body("```mermaid\ngraph TD\n```");
        assert_eq!(
            out,
            "<pre><code class=\"language-mermaid\">graph TD\n</code></pre>\n"
        );
    }

    #[test]
    fn fragment_uses_inline_styles_and_no_wrapper() {
        let out = frag("==a== `c`\n\n```\nx\n```\n\n| h |\n|---|\n| v |");
        assert!(out.starts_with("<p><mark style=\"background-color:#FFF3A3\">a</mark> <code style=\"font-family:'Cascadia Mono',Consolas,monospace\">c</code></p>\n"));
        assert!(out.contains(
            "<pre style=\"font-family:'Cascadia Mono',Consolas,monospace\"><code>x\n</code></pre>"
        ));
        assert!(out.contains("<table border=\"1\" cellspacing=\"0\" cellpadding=\"4\">"));
        assert!(!out.contains("<html"));
    }

    #[test]
    fn xhtml_flavor_self_closes_void_elements() {
        let md = "a  \nb\n\n---\n\n- [x] t\n- [ ] u\n\n![i](https://x/y.png)\n\nz[^1]\n\n[^1]: n";
        let doc = parse_document(md);
        let mut r = Renderer::new(
            &doc.blocks,
            md,
            &NoAssets,
            Flavor::Xhtml,
            ImageMode::DataUri,
        );
        let mut out = String::new();
        r.blocks(&doc.blocks, &mut out);
        r.footnotes_section(&mut out);
        assert!(out.contains("<br />\n"));
        assert!(out.contains("<hr />\n"));
        assert!(
            out.contains("<input type=\"checkbox\" disabled=\"disabled\" checked=\"checked\" />")
        );
        assert!(out.contains("<input type=\"checkbox\" disabled=\"disabled\" />"));
        assert!(out.contains("<img src=\"https://x/y.png\" alt=\"i\" />"));
        assert!(out.contains("<section class=\"footnotes\">\n<hr />\n<ol>"));
    }

    #[test]
    fn resource_mode_collects_each_image_once() {
        let md = "![a](pic.png) ![b](pic.png)";
        let doc = parse_document(md);
        let mut r = Renderer::new(&doc.blocks, md, &Fake, Flavor::Xhtml, ImageMode::Resources);
        let mut out = String::new();
        r.blocks(&doc.blocks, &mut out);
        assert_eq!(r.resources.len(), 1);
        assert_eq!(r.resources[0].path, "images/image-1.png");
        assert_eq!(r.resources[0].bytes, vec![1, 2, 3]);
        assert_eq!(out.matches("images/image-1.png").count(), 2);
        assert_eq!(r.headings.len(), 0);
    }

    #[test]
    fn heading_info_is_collected() {
        let md = "# One\n\n## Two *x*";
        let doc = parse_document(md);
        let mut r = Renderer::new(
            &doc.blocks,
            md,
            &NoAssets,
            Flavor::Html5,
            ImageMode::DataUri,
        );
        let mut out = String::new();
        r.blocks(&doc.blocks, &mut out);
        assert_eq!(
            r.headings,
            vec![
                HeadingInfo {
                    level: 1,
                    id: "one".into(),
                    text: "One".into()
                },
                HeadingInfo {
                    level: 2,
                    id: "two-x".into(),
                    text: "Two x".into()
                },
            ]
        );
    }
}
