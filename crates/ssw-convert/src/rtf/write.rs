//! Markdown → RTF.
//!
//! The output is 7-bit ASCII: every non-ASCII character is written as `\uN?` (with `\uc1`).
//! Fonts: `\f0` Segoe UI (body) and `\f1` Cascadia Mono (code). Colors: 1 link accent, 2–7
//! highlights (yellow, red, orange, green, blue, purple), 8 code background, 9 muted text.
//! Headings carry `\outlinelevelN`; lists use `\listtable` + `\listoverride` plus the legacy
//! `\pntext`/`\pn` groups; tables use `\trowd`; links are `HYPERLINK` fields; PNG and JPEG
//! images are embedded as `\pngblip`/`\jpegblip`. Math and diagrams (no SVG in RTF) appear as
//! their source in the code font.

use std::collections::HashMap;
use std::fmt::Write as _;

use ssw_core::markdown::ast::{
    Alignment, Block, BlockKind, HighlightColor, Image, Inline, InlineKind, Table, TableRow,
};
use ssw_core::markdown::definitions::normalize_label;
use ssw_core::markdown::{parse_document, unescape_markdown};

use crate::assets::{AssetProvider, is_external_url};
use crate::txt::inline_plain;

/// Page width available to tables, in twips (A4/Letter with 1" margins is about 9000).
const TABLE_WIDTH: usize = 9000;

/// RTF text for Markdown source; local images are loaded through `provider`.
pub fn markdown_to_rtf(src: &str, provider: &dyn AssetProvider) -> String {
    let doc = parse_document(src);
    let mut w = Writer::new(&doc.blocks, src, provider);
    w.blocks(&doc.blocks, 0);
    let mut out = String::with_capacity(w.body.len() + 4096);
    out.push_str("{\\rtf1\\ansi\\ansicpg1252\\uc1\\deff0\\deflang1033\n");
    out.push_str(
        "{\\fonttbl{\\f0\\fswiss\\fcharset0 Segoe UI;}{\\f1\\fmodern\\fcharset0 Cascadia Mono;}}\n",
    );
    out.push_str("{\\colortbl;\\red24\\green116\\blue211;\\red255\\green243\\blue163;\\red255\\green201\\blue201;\\red255\\green216\\blue168;\\red195\\green240\\blue200;\\red197\\green221\\blue251;\\red227\\green208\\blue248;\\red243\\green243\\blue243;\\red107\\green107\\blue107;}\n");
    write_list_tables(&mut out, &w.lists);
    out.push_str(&w.body);
    out.push_str("}\n");
    out
}

/// One list instance: ordered or bullet, and the number of its first item.
#[derive(Debug, Clone, Copy)]
struct ListInstance {
    ordered: bool,
    start: u32,
}

fn write_list_tables(out: &mut String, lists: &[ListInstance]) {
    out.push_str("{\\*\\listtable");
    for (id, ordered) in [(1usize, false), (2usize, true)] {
        let _ = write!(out, "{{\\list\\listtemplateid{id}\\listhybrid");
        for level in 0..9usize {
            let indent = 720 * (level + 1);
            if ordered {
                let _ = write!(
                    out,
                    "{{\\listlevel\\levelnfc0\\leveljc0\\levelstartat1\\levelfollow0{{\\leveltext\\leveltemplateid{}\\'02\\'{level:02x}.;}}{{\\levelnumbers\\'01;}}\\fi-360\\li{indent}}}",
                    id * 100 + level
                );
            } else {
                let _ = write!(
                    out,
                    "{{\\listlevel\\levelnfc23\\leveljc0\\levelstartat1\\levelfollow0{{\\leveltext\\leveltemplateid{}\\'01\\u8226?;}}{{\\levelnumbers;}}\\fi-360\\li{indent}}}",
                    id * 100 + level
                );
            }
        }
        let _ = write!(out, "{{\\listname ;}}\\listid{id}}}");
    }
    out.push_str("}\n{\\*\\listoverridetable");
    for (i, list) in lists.iter().enumerate() {
        let id = if list.ordered { 2 } else { 1 };
        let ls = i + 1;
        if list.ordered && list.start != 1 {
            let _ = write!(
                out,
                "{{\\listoverride\\listid{id}\\listoverridecount1{{\\lfolevel\\listoverridestartat\\levelstartat{}}}\\ls{ls}}}",
                list.start
            );
        } else {
            let _ = write!(
                out,
                "{{\\listoverride\\listid{id}\\listoverridecount0\\ls{ls}}}"
            );
        }
    }
    out.push_str("}\n");
}

/// Appends `s` as RTF text: control characters escaped, non-ASCII as `\uN?`.
pub(crate) fn push_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '\t' => out.push_str("\\tab "),
            '\n' => out.push_str("\\line "),
            c if (' '..='~').contains(&c) => out.push(c),
            c if c.is_control() => {}
            c => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{}?", *u as i16);
                }
            }
        }
    }
}

fn highlight_index(color: Option<HighlightColor>) -> usize {
    match color {
        None | Some(HighlightColor::Yellow) => 2,
        Some(HighlightColor::Red) => 3,
        Some(HighlightColor::Orange) => 4,
        Some(HighlightColor::Green) => 5,
        Some(HighlightColor::Blue) => 6,
        Some(HighlightColor::Purple) => 7,
    }
}

/// Pixel size of a PNG or JPEG.
fn image_size(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    match mime {
        "image/png" => {
            let ihdr = bytes.get(16..24)?;
            let w = u32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]);
            let h = u32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]);
            Some((w, h))
        }
        "image/jpeg" => {
            let mut i = 2;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
                if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                    let h = u32::from(u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]));
                    let w = u32::from(u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]));
                    return Some((w, h));
                }
                i += 2 + len.max(2);
            }
            None
        }
        _ => None,
    }
}

struct Writer<'a> {
    src: &'a str,
    provider: &'a dyn AssetProvider,
    body: String,
    lists: Vec<ListInstance>,
    footnote_defs: HashMap<String, &'a Block>,
}

impl<'a> Writer<'a> {
    fn new(all: &'a [Block], src: &'a str, provider: &'a dyn AssetProvider) -> Self {
        let mut footnote_defs = HashMap::new();
        let mut stack: Vec<&'a Block> = all.iter().rev().collect();
        while let Some(b) = stack.pop() {
            if let BlockKind::FootnoteDefinition { label } = &b.kind {
                footnote_defs.entry(normalize_label(label)).or_insert(b);
            }
            stack.extend(b.children.iter().rev());
        }
        Writer {
            src,
            provider,
            body: String::new(),
            lists: Vec::new(),
            footnote_defs,
        }
    }

    /// Start of a paragraph: reset formatting, body font, then `props`.
    fn pard(&mut self, props: &str) {
        self.body.push_str("\\pard\\plain\\f0\\fs22");
        self.body.push_str(props);
        self.body.push(' ');
    }

    fn blocks(&mut self, blocks: &'a [Block], quote: usize) {
        for b in blocks {
            self.block(b, quote, 0);
        }
    }

    /// `quote` is the block-quote nesting depth (left indent); `depth` the list nesting.
    fn block(&mut self, b: &'a Block, quote: usize, depth: usize) {
        let indent = quote * 567;
        match &b.kind {
            BlockKind::Heading { level, .. } => {
                let lv = usize::from((*level).clamp(1, 6));
                let size = [38, 32, 28, 24, 22, 22][lv - 1];
                self.pard(&format!(
                    "\\sb240\\sa120\\keepn\\outlinelevel{}\\b\\fs{size}",
                    lv - 1
                ));
                self.inlines(&b.inlines);
                self.body.push_str("\\par\n");
            }
            BlockKind::Paragraph => {
                self.pard(&format!("\\sa160\\li{indent}"));
                self.inlines(&b.inlines);
                self.body.push_str("\\par\n");
            }
            BlockKind::List { ordered, start, .. } => {
                self.lists.push(ListInstance {
                    ordered: *ordered,
                    start: *start,
                });
                let ls = self.lists.len();
                for (i, item) in b.children.iter().enumerate() {
                    self.list_item(
                        item,
                        *ordered,
                        u64::from(*start) + i as u64,
                        ls,
                        depth,
                        quote,
                    );
                }
            }
            BlockKind::ListItem { .. } => self.blocks_in(&b.children, quote, depth),
            BlockKind::BlockQuote => {
                for child in &b.children {
                    self.quoted(child, quote + 1, depth);
                }
            }
            BlockKind::Callout { kind, title, .. } => {
                self.pard(&format!(
                    "\\sa60\\li{}\\brdrl\\brdrs\\brdrw30\\brsp100\\b",
                    indent + 567
                ));
                match title.and_then(|t| t.get(self.src)) {
                    Some(t) => push_text(&mut self.body, t),
                    None => push_text(&mut self.body, kind.keyword()),
                }
                self.body.push_str("\\par\n");
                for child in &b.children {
                    self.quoted(child, quote + 1, depth);
                }
            }
            BlockKind::CodeBlock(cb) => {
                let text = cb.text(self.src);
                self.code(text.trim_end_matches('\n'), indent);
            }
            BlockKind::MathBlock(m) => {
                let text = m.text(self.src);
                self.code(&text, indent);
            }
            BlockKind::Table(t) => self.table(t),
            BlockKind::ThematicBreak => {
                self.pard("\\sa160\\brdrb\\brdrs\\brdrw10\\brsp20");
                self.body.push_str("\\par\n");
            }
            BlockKind::HtmlBlock(h) => {
                let text = h.text(self.src);
                self.code(text.trim_end_matches('\n'), indent);
            }
            BlockKind::PageBreak => self.body.push_str("\\page\n"),
            BlockKind::FrontMatter { .. }
            | BlockKind::FootnoteDefinition { .. }
            | BlockKind::LinkReferenceDefinition(_) => {}
        }
    }

    fn blocks_in(&mut self, blocks: &'a [Block], quote: usize, depth: usize) {
        for b in blocks {
            self.block(b, quote, depth);
        }
    }

    /// A child of a quote/callout: paragraphs get the quote indent and a left rule.
    fn quoted(&mut self, b: &'a Block, quote: usize, depth: usize) {
        if b.kind == BlockKind::Paragraph {
            self.pard(&format!(
                "\\sa120\\li{}\\brdrl\\brdrs\\brdrw30\\brsp100\\cf9",
                quote * 567
            ));
            self.inlines(&b.inlines);
            self.body.push_str("\\par\n");
        } else {
            self.block(b, quote, depth);
        }
    }

    fn code(&mut self, text: &str, indent: usize) {
        let lines: Vec<&str> = text.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            let after = if i + 1 == lines.len() {
                "\\sa160"
            } else {
                "\\sa0"
            };
            self.pard(&format!("{after}\\li{indent}\\cbpat8\\f1\\fs20"));
            push_text(&mut self.body, line);
            self.body.push_str("\\par\n");
        }
    }

    fn list_item(
        &mut self,
        item: &'a Block,
        ordered: bool,
        number: u64,
        ls: usize,
        depth: usize,
        quote: usize,
    ) {
        let task = match &item.kind {
            BlockKind::ListItem { task, .. } => *task,
            _ => None,
        };
        let left = 720 * (depth + 1) + quote * 567;
        let mut first = true;
        for child in &item.children {
            if first && child.kind == BlockKind::Paragraph {
                first = false;
                self.pard(&format!("\\sa60\\ls{ls}\\ilvl{depth}\\fi-360\\li{left}"));
                if ordered {
                    let _ = write!(
                        self.body,
                        "{{\\pntext\\f0 {number}.\\tab}}{{\\*\\pn\\pnlvlbody\\pnindent360\\pnstart{number}\\pndec{{\\pntxta.}}}}"
                    );
                } else {
                    self.body.push_str("{\\pntext\\f0\\u8226?\\tab}{\\*\\pn\\pnlvlblt\\pnf0\\pnindent360{\\pntxtb\\u8226?}}");
                }
                if let Some(t) = task {
                    self.body
                        .push_str(if t.checked { "\\u9745? " } else { "\\u9744? " });
                }
                self.inlines(&child.inlines);
                self.body.push_str("\\par\n");
            } else {
                first = false;
                self.block(child, quote, depth + 1);
            }
        }
    }

    fn table(&mut self, t: &'a Table) {
        let cols = std::iter::once(&t.header)
            .chain(t.rows.iter())
            .map(|r| r.cells.len())
            .max()
            .unwrap_or(1)
            .max(1);
        self.table_row(&t.header, &t.alignments, cols, true);
        for row in &t.rows {
            self.table_row(row, &t.alignments, cols, false);
        }
        self.pard("\\sa120");
        self.body.push_str("\\par\n");
    }

    fn table_row(&mut self, row: &'a TableRow, aligns: &[Alignment], cols: usize, header: bool) {
        self.body.push_str("\\trowd\\trgaph108\\trleft0");
        if header {
            self.body.push_str("\\trhdr");
        }
        let width = TABLE_WIDTH / cols;
        for i in 0..cols {
            self.body.push_str("\\clbrdrt\\brdrs\\brdrw10\\clbrdrl\\brdrs\\brdrw10\\clbrdrb\\brdrs\\brdrw10\\clbrdrr\\brdrs\\brdrw10");
            if header {
                self.body.push_str("\\clcbpat8");
            }
            let _ = write!(self.body, "\\cellx{}", width * (i + 1));
        }
        self.body.push('\n');
        for i in 0..cols {
            let align = match aligns.get(i) {
                Some(Alignment::Center) => "\\qc",
                Some(Alignment::Right) => "\\qr",
                _ => "\\ql",
            };
            self.pard(&format!("\\intbl{align}"));
            if header {
                self.body.push_str("\\b ");
            }
            if let Some(cell) = row.cells.get(i) {
                self.inlines(&cell.inlines);
            }
            self.body.push_str("\\cell\n");
        }
        self.body.push_str("\\row\n");
    }

    fn inlines(&mut self, items: &'a [Inline]) {
        for n in items {
            self.inline(n);
        }
    }

    fn group(&mut self, open: &str, n: &'a Inline) {
        self.body.push('{');
        self.body.push_str(open);
        self.body.push(' ');
        self.inlines(&n.children);
        self.body.push('}');
    }

    fn inline(&mut self, n: &'a Inline) {
        let src = self.src;
        match &n.kind {
            InlineKind::Text | InlineKind::RawHtml => {
                push_text(&mut self.body, n.range.get(src).unwrap_or(""))
            }
            InlineKind::Escape | InlineKind::InlineMath | InlineKind::WikiLink(_) => {
                push_text(&mut self.body, n.content.get(src).unwrap_or(""));
            }
            InlineKind::Entity { decoded } => push_text(&mut self.body, decoded),
            InlineKind::Emphasis => self.group("\\i", n),
            InlineKind::Strong => self.group("\\b", n),
            InlineKind::Underline => self.group("\\ul", n),
            InlineKind::Strikethrough => self.group("\\strike", n),
            InlineKind::Highlight { color } => {
                let open = format!("\\highlight{}", highlight_index(*color));
                self.group(&open, n);
            }
            InlineKind::Code { text } => {
                self.body.push_str("{\\f1\\fs20 ");
                push_text(&mut self.body, text);
                self.body.push('}');
            }
            InlineKind::Link(l) => {
                self.body.push_str("{\\field{\\*\\fldinst{HYPERLINK \"");
                push_text(&mut self.body, &l.dest.replace('"', "%22"));
                self.body.push_str("\"}}{\\fldrslt{\\cf1\\ul ");
                self.inlines(&n.children);
                self.body.push_str("}}}");
            }
            InlineKind::Image(img) => self.image(n, img),
            InlineKind::FootnoteRef { label } => self.footnote(label),
            InlineKind::HardBreak => self.body.push_str("\\line "),
            InlineKind::SoftBreak => self.body.push(' '),
        }
    }

    fn footnote(&mut self, label: &str) {
        let def = self.footnote_defs.get(&normalize_label(label)).copied();
        let mut text = String::new();
        if let Some(def) = def {
            for child in &def.children {
                if !text.is_empty() {
                    text.push(' ');
                }
                inline_plain(&child.inlines, self.src, " ", false, &mut text);
            }
        }
        self.body
            .push_str("{\\super\\chftn}{\\footnote\\pard\\plain\\f0\\fs20 {\\super\\chftn} ");
        push_text(&mut self.body, &text);
        self.body.push_str("\\par}");
    }

    fn image(&mut self, n: &'a Inline, img: &'a Image) {
        let alt = if n.children.is_empty() {
            unescape_markdown(img.alt.get(self.src).unwrap_or(""))
        } else {
            let mut s = String::new();
            inline_plain(&n.children, self.src, " ", false, &mut s);
            s
        };
        let loaded = if is_external_url(&img.dest) {
            None
        } else {
            self.provider.image(&img.dest)
        };
        let embedded = loaded.and_then(|(bytes, mime)| {
            let blip = match mime {
                "image/png" => "\\pngblip",
                "image/jpeg" => "\\jpegblip",
                _ => return None,
            };
            let (w, h) = image_size(&bytes, mime)?;
            Some((bytes, blip, w, h))
        });
        match embedded {
            Some((bytes, blip, w, h)) => {
                let (gw, gh) = match (img.width, img.height) {
                    (Some(iw), Some(ih)) => (iw, ih),
                    (Some(iw), None) => (
                        iw,
                        (u64::from(iw) * u64::from(h) / u64::from(w.max(1))) as u32,
                    ),
                    _ => (w, h),
                };
                let _ = write!(
                    self.body,
                    "{{\\pict{blip}\\picw{w}\\pich{h}\\picwgoal{}\\pichgoal{} ",
                    u64::from(gw) * 15,
                    u64::from(gh) * 15
                );
                for b in &bytes {
                    let _ = write!(self.body, "{b:02x}");
                }
                self.body.push('}');
            }
            None => {
                self.body.push('[');
                push_text(&mut self.body, &alt);
                self.body.push(']');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::NoAssets;

    fn rtf(md: &str) -> String {
        markdown_to_rtf(md, &NoAssets)
    }

    fn body_of(md: &str) -> String {
        let out = rtf(md);
        let at = out.find("{\\*\\listoverridetable").unwrap();
        let after = &out[at..];
        let nl = after.find("}\n").unwrap();
        out[at + nl + 2..out.len() - 2].to_string()
    }

    #[test]
    fn header_has_fonts_colors_and_balanced_braces() {
        let out = rtf("# T\n\nhello {x} \\ ğ 😀 \t\n");
        assert!(out.starts_with("{\\rtf1\\ansi\\ansicpg1252\\uc1\\deff0"));
        assert!(out.contains("Cascadia Mono;}"));
        assert!(out.contains("{\\colortbl;\\red24\\green116\\blue211;"));
        assert!(out.is_ascii());
        let mut depth = 0i32;
        let mut prev = ' ';
        for c in out.chars() {
            match c {
                '{' if prev != '\\' => depth += 1,
                '}' if prev != '\\' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0);
            prev = if prev == '\\' && c == '\\' { ' ' } else { c };
        }
        assert_eq!(depth, 0);
    }

    #[test]
    fn text_escaping_table() {
        let table = [
            ("a\\b", "a\\\\b"),
            ("{x}", "\\{x\\}"),
            ("ğ", "\\u287?"),
            ("日", "\\u26085?"),
            ("😀", "\\u-10179?\\u-8704?"),
            ("a\tb", "a\\tab b"),
            ("a\nb", "a\\line b"),
            ("\u{1}x", "x"),
        ];
        for (input, expected) in table {
            let mut out = String::new();
            push_text(&mut out, input);
            assert_eq!(out, expected, "{input:?}");
        }
    }

    #[test]
    fn headings_have_sizes_and_outline_levels() {
        let b = body_of("# A\n\n## B\n\n###### F");
        assert!(b.contains("\\outlinelevel0\\b\\fs38 A\\par"));
        assert!(b.contains("\\outlinelevel1\\b\\fs32 B\\par"));
        assert!(b.contains("\\outlinelevel5\\b\\fs22 F\\par"));
    }

    #[test]
    fn inline_formats_links_and_breaks() {
        let b = body_of("**b** *i* ~u~ ~~s~~ ==h== =={green}g== `c` [l](http://x/a\"b) a  \nb\nc");
        for wanted in [
            "{\\b b}",
            "{\\i i}",
            "{\\ul u}",
            "{\\strike s}",
            "{\\highlight2 h}",
            "{\\highlight5 g}",
            "{\\f1\\fs20 c}",
            "{\\field{\\*\\fldinst{HYPERLINK \"http://x/a%22b\"}}{\\fldrslt{\\cf1\\ul l}}}",
            "a\\line b c",
        ] {
            assert!(b.contains(wanted), "{wanted} in {b}");
        }
    }

    #[test]
    fn lists_have_overrides_legacy_groups_and_tasks() {
        let md = "- a\n  - n\n- [x] done\n- [ ] open\n\n3. x\n4. y\n\npara\n\n1. z";
        let out = rtf(md);
        assert!(out.contains("{\\*\\listtable{\\list\\listtemplateid1\\listhybrid"));
        assert!(out.contains("\\levelnfc23"));
        assert!(out.contains("\\levelnfc0"));
        assert!(out.contains("{\\listoverride\\listid1\\listoverridecount0\\ls1}"));
        assert!(out.contains("{\\listoverride\\listid1\\listoverridecount0\\ls2}"));
        assert!(out.contains("{\\listoverride\\listid2\\listoverridecount1{\\lfolevel\\listoverridestartat\\levelstartat3}\\ls3}"));
        assert!(out.contains("{\\listoverride\\listid2\\listoverridecount0\\ls4}"));
        let b = body_of(md);
        assert!(
            b.contains("\\ls1\\ilvl0\\fi-360\\li720 {\\pntext\\f0\\u8226?\\tab}{\\*\\pn\\pnlvlblt")
        );
        assert!(b.contains("\\ls2\\ilvl1\\fi-360\\li1440"));
        assert!(b.contains("\\u9745? done"));
        assert!(b.contains("\\u9744? open"));
        assert!(b.contains(
            "{\\pntext\\f0 3.\\tab}{\\*\\pn\\pnlvlbody\\pnindent360\\pnstart3\\pndec{\\pntxta.}}"
        ));
        assert!(b.contains("{\\pntext\\f0 4.\\tab}"));
    }

    #[test]
    fn tables_use_trowd_cells_and_alignment() {
        let b = body_of("| a | b |\n|:-:|--:|\n| 1 | 2 |");
        assert_eq!(b.matches("\\trowd").count(), 2);
        assert_eq!(b.matches("\\row").count(), 2);
        assert!(b.contains("\\trhdr"));
        assert!(b.contains("\\cellx4500\\clbrdrt"));
        assert!(b.contains("\\cellx9000"));
        assert!(b.contains("\\intbl\\qc \\b a\\cell"));
        assert!(b.contains("\\intbl\\qr \\b b\\cell"));
        assert!(b.contains("\\intbl\\qc 1\\cell"));
    }

    #[test]
    fn ragged_rows_are_padded_with_empty_cells() {
        let b = body_of("| a | b |\n|---|---|\n| 1 |");
        assert_eq!(b.matches("\\cell\n").count(), 4);
    }

    #[test]
    fn blocks_quote_callout_code_math_rule_pagebreak_html() {
        let b = body_of(
            "> q\n\n> [!NOTE] Hi\n> body\n\n```\nl1\nl2\n```\n\n$$\nx\n$$\n\n---\n\n<!-- pagebreak -->\n\n<b>raw</b>",
        );
        assert!(b.contains("\\li567\\brdrl\\brdrs\\brdrw30\\brsp100\\cf9 q\\par"));
        assert!(b.contains("\\b Hi\\par"));
        assert!(b.contains("\\cf9 body\\par"));
        assert!(b.contains("\\sa0\\li0\\cbpat8\\f1\\fs20 l1\\par"));
        assert!(b.contains("\\sa160\\li0\\cbpat8\\f1\\fs20 l2\\par"));
        assert!(b.contains("\\cbpat8\\f1\\fs20 x\\par"));
        assert!(b.contains("\\brdrb\\brdrs\\brdrw10"));
        assert!(b.contains("\\page\n"));
        assert!(b.contains("<b>raw<"));
    }

    #[test]
    fn callout_without_title_uses_keyword_and_quote_keeps_code_blocks() {
        let b = body_of("> [!TIP]\n> ```\n> c\n> ```");
        assert!(b.contains("\\b TIP\\par"));
        assert!(b.contains("\\li567\\cbpat8\\f1\\fs20 c\\par"));
    }

    #[test]
    fn footnotes_use_footnote_destination() {
        let b = body_of("x[^1] y[^q]\n\n[^1]: the *note*\n\n    second");
        assert!(b.contains("{\\super\\chftn}{\\footnote\\pard\\plain\\f0\\fs20 {\\super\\chftn} the note second\\par}"), "{b}");
        assert!(b.contains("[^q]"));
    }

    struct Pics;
    impl AssetProvider for Pics {
        fn image(&self, path: &str) -> Option<(Vec<u8>, &'static str)> {
            match path {
                "p.png" => {
                    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
                    png.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0]);
                    Some((png, "image/png"))
                }
                "j.jpg" => Some((
                    vec![
                        0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0, 0xFF, 0xC0, 0, 11, 8, 0, 20, 0, 40, 3,
                        1, 0, 0,
                    ],
                    "image/jpeg",
                )),
                "noise.jpg" => Some((
                    vec![0xFF, 0xD8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
                    "image/jpeg",
                )),
                "g.gif" => Some((b"GIF89a".to_vec(), "image/gif")),
                _ => None,
            }
        }
        fn math_svg(&self, _: &str, _: bool) -> Option<String> {
            None
        }
        fn mermaid_svg(&self, _: &str) -> Option<String> {
            None
        }
    }

    #[test]
    fn images_embed_png_and_jpeg_with_sizes_and_fall_back_to_alt() {
        let md = "![a](p.png) ![b|10](p.png) ![c|7x9](p.png) ![j](j.jpg) ![n](noise.jpg) ![g](g.gif) ![x](missing.png) ![w](https://h/x.png)";
        let out = markdown_to_rtf(md, &Pics);
        assert!(out.contains("{\\pict\\pngblip\\picw2\\pich3\\picwgoal30\\pichgoal45 89504e470d0a1a0a0000000d49484452"));
        assert!(out.contains("\\picwgoal150\\pichgoal225 "));
        assert!(out.contains("\\picwgoal105\\pichgoal135 "));
        assert!(out.contains("{\\pict\\jpegblip\\picw40\\pich20\\picwgoal600\\pichgoal300 "));
        for alt in ["[n]", "[g]", "[x]", "[w]"] {
            assert!(out.contains(alt), "{alt}");
        }
    }

    #[test]
    fn image_size_edge_cases() {
        assert_eq!(image_size(b"short", "image/png"), None);
        assert_eq!(image_size(b"", "image/jpeg"), None);
        assert_eq!(image_size(b"x", "image/gif"), None);
    }
}
