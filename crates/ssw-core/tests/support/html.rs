//! AST → CommonMark-style HTML, used to check the parser against the CommonMark and GFM
//! example suites. Mirrors the reference renderer's whitespace rules.

use ssw_core::markdown::ast::{
    Alignment, Block, BlockKind, Document, Inline, InlineKind, TableRow,
};
use ssw_core::markdown::unescape_markdown;

pub struct Options {
    /// Escape the `<` of the GFM-filtered raw HTML tags.
    pub tagfilter: bool,
}

pub fn render(doc: &Document, src: &str, options: &Options) -> String {
    let mut r = Renderer {
        src,
        out: String::new(),
        options,
    };
    r.blocks(&doc.blocks, false);
    r.out
}

struct Renderer<'a> {
    src: &'a str,
    out: String,
    options: &'a Options,
}

fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
}

fn encode_url(url: &str) -> String {
    let b = url.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let keep = c.is_ascii_alphanumeric() || b";/?:@&=+$,-_.!~*'()#".contains(&c);
        if keep {
            out.push(char::from(c));
        } else if c == b'%'
            && b.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            out.push('%');
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
        i += 1;
    }
    out
}

const FILTERED: &[&str] = &[
    "title",
    "textarea",
    "style",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "script",
    "plaintext",
];

impl Renderer<'_> {
    fn cr(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn raw(&mut self, text: &str) {
        if !self.options.tagfilter {
            self.out.push_str(text);
            return;
        }
        let lower = text.to_ascii_lowercase();
        let b = text.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'<' {
                let rest = &lower[i + 1..];
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                let hit = FILTERED.iter().any(|name| {
                    rest.starts_with(name)
                        && matches!(
                            rest.as_bytes().get(name.len()),
                            None | Some(b' ' | b'\t' | b'\n' | b'>' | b'/')
                        )
                });
                if hit {
                    self.out.push_str("&lt;");
                    i += 1;
                    continue;
                }
            }
            let ch_len = text[i..].chars().next().map_or(1, char::len_utf8);
            self.out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }

    fn blocks(&mut self, blocks: &[Block], tight: bool) {
        for block in blocks {
            self.block(block, tight);
        }
    }

    fn text(&self, range: ssw_core::text::TextRange) -> &str {
        range.get(self.src).unwrap_or("")
    }

    fn block(&mut self, block: &Block, tight: bool) {
        match &block.kind {
            BlockKind::Heading { level, .. } => {
                self.cr();
                self.out.push_str(&format!("<h{level}>"));
                self.inlines(&block.inlines);
                self.out.push_str(&format!("</h{level}>\n"));
            }
            BlockKind::Paragraph => {
                if tight {
                    self.inlines(&block.inlines);
                } else {
                    self.cr();
                    self.out.push_str("<p>");
                    self.inlines(&block.inlines);
                    self.out.push_str("</p>\n");
                }
            }
            BlockKind::List {
                ordered,
                start,
                tight,
                ..
            } => {
                self.cr();
                if *ordered {
                    if *start == 1 {
                        self.out.push_str("<ol>\n");
                    } else {
                        self.out.push_str(&format!("<ol start=\"{start}\">\n"));
                    }
                } else {
                    self.out.push_str("<ul>\n");
                }
                self.blocks(&block.children, *tight);
                self.cr();
                self.out
                    .push_str(if *ordered { "</ol>\n" } else { "</ul>\n" });
            }
            BlockKind::ListItem { task, .. } => {
                self.out.push_str("<li>");
                for (n, child) in block.children.iter().enumerate() {
                    if n == 0
                        && let Some(t) = task
                    {
                        if !tight {
                            self.cr();
                            self.out.push_str("<p>");
                        }
                        self.out.push_str(if t.checked {
                            "<input type=\"checkbox\" checked=\"\" disabled=\"\" /> "
                        } else {
                            "<input type=\"checkbox\" disabled=\"\" /> "
                        });
                        self.inlines(&child.inlines);
                        if !tight {
                            self.out.push_str("</p>\n");
                        }
                        continue;
                    }
                    self.block(child, tight);
                }
                self.out.push_str("</li>\n");
            }
            BlockKind::BlockQuote | BlockKind::Callout { .. } => {
                self.cr();
                if let BlockKind::Callout { kind, .. } = &block.kind {
                    self.out.push_str(&format!(
                        "<blockquote class=\"callout {}\">\n",
                        kind.keyword().to_ascii_lowercase()
                    ));
                } else {
                    self.out.push_str("<blockquote>\n");
                }
                self.blocks(&block.children, false);
                self.cr();
                self.out.push_str("</blockquote>\n");
            }
            BlockKind::CodeBlock(code) => {
                self.cr();
                let lang = code.info.and_then(|i| {
                    let info = unescape_markdown(self.text(i));
                    info.split([' ', '\t'])
                        .next()
                        .filter(|w| !w.is_empty())
                        .map(str::to_string)
                });
                match lang {
                    Some(l) => {
                        self.out.push_str("<pre><code class=\"language-");
                        escape(&l, &mut self.out);
                        self.out.push_str("\">");
                    }
                    None => self.out.push_str("<pre><code>"),
                }
                escape(&code.text(self.src), &mut self.out);
                self.out.push_str("</code></pre>\n");
            }
            BlockKind::MathBlock(math) => {
                self.cr();
                self.out.push_str("<div class=\"math\">");
                escape(&math.text(self.src), &mut self.out);
                self.out.push_str("</div>\n");
            }
            BlockKind::Table(table) => {
                self.cr();
                self.out.push_str("<table>\n<thead>\n");
                self.row(
                    &table.header,
                    &table.alignments,
                    "th",
                    table.alignments.len(),
                );
                self.out.push_str("</thead>\n");
                if !table.rows.is_empty() {
                    self.out.push_str("<tbody>\n");
                    for row in &table.rows {
                        self.row(row, &table.alignments, "td", table.alignments.len());
                    }
                    self.out.push_str("</tbody>\n");
                }
                self.out.push_str("</table>\n");
            }
            BlockKind::ThematicBreak => {
                self.cr();
                self.out.push_str("<hr />\n");
            }
            BlockKind::FrontMatter { .. }
            | BlockKind::LinkReferenceDefinition(_)
            | BlockKind::FootnoteDefinition { .. } => {}
            BlockKind::HtmlBlock(html) => {
                self.cr();
                let text = html.text(self.src);
                self.raw(&text);
            }
            BlockKind::PageBreak => {
                self.cr();
                let t = self.text(block.range).to_string();
                self.out.push_str(&t);
                self.out.push('\n');
            }
        }
    }

    fn row(&mut self, row: &TableRow, aligns: &[Alignment], tag: &str, columns: usize) {
        self.out.push_str("<tr>\n");
        for col in 0..columns {
            let align = match aligns.get(col) {
                Some(Alignment::Left) => " align=\"left\"",
                Some(Alignment::Center) => " align=\"center\"",
                Some(Alignment::Right) => " align=\"right\"",
                _ => "",
            };
            self.out.push_str(&format!("<{tag}{align}>"));
            if let Some(cell) = row.cells.get(col) {
                self.inlines(&cell.inlines);
            }
            self.out.push_str(&format!("</{tag}>\n"));
        }
        self.out.push_str("</tr>\n");
    }

    fn alt_text(&self, inlines: &[Inline], out: &mut String) {
        for i in inlines {
            match &i.kind {
                InlineKind::Text => out.push_str(self.text(i.range)),
                InlineKind::Escape => out.push_str(self.text(i.content)),
                InlineKind::Entity { decoded } => out.push_str(decoded),
                InlineKind::Code { text } => out.push_str(text),
                InlineKind::SoftBreak | InlineKind::HardBreak => out.push('\n'),
                InlineKind::RawHtml | InlineKind::FootnoteRef { .. } => {}
                _ => self.alt_text(&i.children, out),
            }
        }
    }

    fn inlines(&mut self, inlines: &[Inline]) {
        for inline in inlines {
            self.inline(inline);
        }
    }

    fn wrap(&mut self, tag: &str, inline: &Inline) {
        self.out.push_str(&format!("<{tag}>"));
        self.inlines(&inline.children);
        self.out.push_str(&format!("</{tag}>"));
    }

    fn inline(&mut self, inline: &Inline) {
        match &inline.kind {
            InlineKind::Text => {
                let t = self.text(inline.range).to_string();
                escape(&t, &mut self.out);
            }
            InlineKind::Escape => {
                let t = self.text(inline.content).to_string();
                escape(&t, &mut self.out);
            }
            InlineKind::Entity { decoded } => escape(decoded, &mut self.out),
            InlineKind::Emphasis => self.wrap("em", inline),
            InlineKind::Strong => self.wrap("strong", inline),
            InlineKind::Underline => self.wrap("u", inline),
            InlineKind::Strikethrough => self.wrap("del", inline),
            InlineKind::Highlight { .. } => self.wrap("mark", inline),
            InlineKind::Code { text } => {
                self.out.push_str("<code>");
                escape(text, &mut self.out);
                self.out.push_str("</code>");
            }
            InlineKind::Link(link) => {
                self.out.push_str("<a href=\"");
                escape(&encode_url(&link.dest), &mut self.out);
                self.out.push('"');
                if let Some(t) = &link.title {
                    self.out.push_str(" title=\"");
                    escape(t, &mut self.out);
                    self.out.push('"');
                }
                self.out.push('>');
                self.inlines(&inline.children);
                self.out.push_str("</a>");
            }
            InlineKind::Image(img) => {
                if matches!(img.kind, ssw_core::markdown::ast::ImageKind::Html) {
                    let t = self.text(inline.range).to_string();
                    self.raw(&t);
                    return;
                }
                self.out.push_str("<img src=\"");
                escape(&encode_url(&img.dest), &mut self.out);
                self.out.push_str("\" alt=\"");
                let mut alt = String::new();
                self.alt_text(&inline.children, &mut alt);
                escape(&alt, &mut self.out);
                self.out.push('"');
                if let Some(t) = &img.title {
                    self.out.push_str(" title=\"");
                    escape(t, &mut self.out);
                    self.out.push('"');
                }
                self.out.push_str(" />");
            }
            InlineKind::WikiLink(w) => {
                self.out.push_str("<a href=\"wikilink:");
                escape(&w.target, &mut self.out);
                self.out.push_str("\">");
                let t = self.text(inline.content).to_string();
                escape(&t, &mut self.out);
                self.out.push_str("</a>");
            }
            InlineKind::FootnoteRef { label } => {
                self.out
                    .push_str("<sup class=\"footnote-ref\"><a href=\"#fn-");
                escape(&encode_url(label), &mut self.out);
                self.out.push_str("\" id=\"fnref-");
                escape(&encode_url(label), &mut self.out);
                self.out.push_str("\" data-footnote-ref>");
                escape(label, &mut self.out);
                self.out.push_str("</a></sup>");
            }
            InlineKind::InlineMath => {
                self.out.push_str("<span class=\"math\">");
                let t = self.text(inline.content).to_string();
                escape(&t, &mut self.out);
                self.out.push_str("</span>");
            }
            InlineKind::HardBreak => self.out.push_str("<br />\n"),
            InlineKind::SoftBreak => self.out.push('\n'),
            InlineKind::RawHtml => {
                let t = self.text(inline.range).to_string();
                self.raw(&t);
            }
        }
    }
}
