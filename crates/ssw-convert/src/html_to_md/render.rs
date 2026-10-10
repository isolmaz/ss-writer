//! Block/inline model of a converted HTML document and its rendering to Markdown.

/// Character formatting that applies to a run of text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Fmt {
    pub(super) bold: bool,
    pub(super) italic: bool,
    pub(super) underline: bool,
    pub(super) strike: bool,
    pub(super) highlight: bool,
    pub(super) code: bool,
    /// Link destination the run belongs to.
    pub(super) link: Option<String>,
}

/// One piece of inline content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Inline {
    Text(String, Fmt),
    /// Image Markdown (`![alt](dest)`), emitted verbatim, optionally inside a link.
    Image {
        markdown: String,
        link: Option<String>,
    },
    /// `<br>`.
    Break,
}

/// Column alignment of a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Align {
    None,
    Center,
    Right,
}

/// A GFM table; `rows[0]` is the header and every row has `align.len()` cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Table {
    pub(super) align: Vec<Align>,
    pub(super) rows: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    /// `Some(checked)` for task list items.
    pub(super) task: Option<bool>,
    pub(super) blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct List {
    pub(super) ordered: bool,
    pub(super) start: u64,
    /// Ordered lists: `)` instead of `.` after the number.
    pub(super) paren: bool,
    pub(super) items: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Block {
    Heading(u8, String),
    /// Final Markdown of the paragraph (inline markup and line-start escapes applied).
    Paragraph(String),
    Code {
        lang: String,
        text: String,
    },
    Quote(Vec<Block>),
    List(List),
    Table(Table),
    Rule,
    PageBreak,
}

/// Renders blocks as a Markdown document: blocks separated by a blank line, one trailing newline.
pub(super) fn render_document(blocks: &[Block]) -> String {
    let mut out = render_blocks(blocks, false);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Fenced code block; the fence is longer than any backtick run inside `text`.
pub(super) fn fenced(text: &str, lang: &str) -> String {
    let fence = "`".repeat(longest_run(text, '`').max(2) + 1);
    format!("{fence}{lang}\n{text}\n{fence}")
}

/// Inline code span; the delimiter is longer than any backtick run inside `text`.
pub(super) fn code_span(text: &str) -> String {
    let ticks = "`".repeat(longest_run(text, '`') + 1);
    let pad = if text.starts_with('`') || text.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{ticks}{pad}{text}{pad}{ticks}")
}

fn longest_run(text: &str, target: char) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == target { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

fn render_blocks(blocks: &[Block], in_item: bool) -> String {
    let mut out = String::new();
    let mut previous: Option<&Block> = None;
    // Marker of the directly preceding list: a list that would reuse it merges with that list.
    let mut previous_marker: Option<(bool, char)> = None;
    for block in blocks {
        let symbol = match block {
            Block::List(list) => {
                let base = list_marker(list, false);
                let marker = match previous_marker {
                    Some((ordered, used)) if ordered == list.ordered && used == base => {
                        list_marker(list, true)
                    }
                    _ => base,
                };
                previous_marker = Some((list.ordered, marker));
                marker
            }
            _ => {
                previous_marker = None;
                '-'
            }
        };
        let rendered = render_block(block, symbol);
        if let Some(prev) = previous {
            let tight =
                in_item && matches!(prev, Block::Paragraph(_)) && matches!(block, Block::List(_));
            out.push_str(if tight { "\n" } else { "\n\n" });
        }
        out.push_str(&rendered);
        previous = Some(block);
    }
    out
}

/// `symbol` is the bullet or number delimiter used when `block` is a list.
fn render_block(block: &Block, symbol: char) -> String {
    match block {
        Block::Heading(level, text) => format!("{} {text}", "#".repeat(usize::from(*level))),
        Block::Paragraph(text) => text.clone(),
        Block::Code { lang, text } => fenced(text, lang),
        Block::Quote(inner) => render_blocks(inner, false)
            .lines()
            .map(|l| {
                if l.is_empty() {
                    ">".to_string()
                } else {
                    format!("> {l}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Block::List(list) => render_list(list, symbol),
        Block::Table(table) => render_table(table),
        Block::Rule => "---".to_string(),
        Block::PageBreak => "<!-- pagebreak -->".to_string(),
    }
}

/// Bullet character or number delimiter of a list; `alternate` picks the other one of the pair.
fn list_marker(list: &List, alternate: bool) -> char {
    match (list.ordered, list.paren != alternate) {
        (true, true) => ')',
        (true, false) => '.',
        (false, _) if alternate => '*',
        (false, _) => '-',
    }
}

fn render_list(list: &List, symbol: char) -> String {
    let loose = list.items.iter().any(|item| {
        item.blocks
            .iter()
            .filter(|b| !matches!(b, Block::List(_)))
            .count()
            > 1
    });
    let mut out = String::new();
    for (index, item) in list.items.iter().enumerate() {
        let marker = if list.ordered {
            format!("{}{symbol} ", list.start + index as u64)
        } else {
            format!("{symbol} ")
        };
        let task = match item.task {
            Some(true) => "[x] ",
            Some(false) => "[ ] ",
            None => "",
        };
        let content = render_blocks(&item.blocks, true);
        let indent = " ".repeat(marker.len());
        let mut lines = content.split('\n');
        let first = format!("{marker}{task}{}", lines.next().unwrap_or_default());
        if index > 0 {
            out.push_str(if loose { "\n\n" } else { "\n" });
        }
        out.push_str(first.trim_end());
        for line in lines {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(&indent);
                out.push_str(line);
            }
        }
    }
    out
}

fn render_table(table: &Table) -> String {
    let line = |cells: &[String]| format!("| {} |", cells.join(" | "));
    let separator = table
        .align
        .iter()
        .map(|a| match a {
            Align::None => "---",
            Align::Center => ":---:",
            Align::Right => "---:",
        })
        .collect::<Vec<_>>()
        .join("|");
    let mut out = line(&table.rows[0]);
    out.push_str(&format!("\n|{separator}|"));
    for row in &table.rows[1..] {
        out.push('\n');
        out.push_str(&line(row));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Inline rendering
// ---------------------------------------------------------------------------------------------

/// Renders inline items as Markdown paragraphs: two or more consecutive `<br>`s start a new
/// paragraph. Paragraphs that render empty are dropped.
pub(super) fn render_paragraphs(items: &[Inline], cell: bool) -> Vec<String> {
    let mut paragraphs: Vec<Vec<Inline>> = Vec::new();
    let mut current: Vec<Inline> = Vec::new();
    let mut breaks = 0;
    for item in normalize(items) {
        if item == Inline::Break {
            breaks += 1;
            continue;
        }
        if breaks >= 2 {
            paragraphs.push(std::mem::take(&mut current));
        } else if breaks == 1 {
            current.push(Inline::Break);
        }
        breaks = 0;
        current.push(item);
    }
    paragraphs.push(current);
    paragraphs
        .iter()
        .map(|p| render_inline(p, cell))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Renders inline items on one logical line: breaks become `<br>` in table cells and hard line
/// breaks elsewhere.
pub(super) fn render_inline(items: &[Inline], cell: bool) -> String {
    let mut emitter = Emitter {
        out: String::new(),
        pending_space: false,
        open: Vec::new(),
        cell,
    };
    for item in normalize(items) {
        match item {
            Inline::Break => {
                emitter.pending_space = false;
                emitter.out.push_str(if cell { "<br>" } else { "\\\n" });
            }
            Inline::Text(text, fmt) => emitter.text(&text, &fmt),
            Inline::Image { markdown, link } => {
                emitter.transition(link.into_iter().map(Mark::Link).collect());
                emitter.out.push_str(&markdown);
            }
        }
    }
    emitter.finish()
}

/// Collapses spaces across item boundaries, drops leading/trailing breaks and spaces, and merges
/// neighbouring text with equal formatting.
fn normalize(items: &[Inline]) -> Vec<Inline> {
    let mut spaced: Vec<Inline> = Vec::with_capacity(items.len());
    let mut at_start = true;
    for item in items {
        match item {
            Inline::Text(text, fmt) => {
                let text = if at_start {
                    text.trim_start_matches(' ')
                } else {
                    text.as_str()
                };
                if text.is_empty() {
                    continue;
                }
                at_start = text.ends_with(' ');
                spaced.push(Inline::Text(text.to_string(), fmt.clone()));
            }
            Inline::Image { .. } => {
                at_start = false;
                spaced.push(item.clone());
            }
            Inline::Break => {
                trim_trailing_space(&mut spaced);
                at_start = true;
                spaced.push(Inline::Break);
            }
        }
    }
    trim_trailing_space(&mut spaced);
    while spaced.last() == Some(&Inline::Break) {
        spaced.pop();
        trim_trailing_space(&mut spaced);
    }
    let leading_breaks = spaced.iter().take_while(|i| **i == Inline::Break).count();
    spaced.drain(..leading_breaks);
    let mut merged: Vec<Inline> = Vec::with_capacity(spaced.len());
    for item in spaced {
        match (merged.last_mut(), &item) {
            (Some(Inline::Text(previous, previous_fmt)), Inline::Text(text, fmt))
                if previous_fmt == fmt =>
            {
                previous.push_str(text);
            }
            _ => merged.push(item),
        }
    }
    merged
}

fn trim_trailing_space(items: &mut Vec<Inline>) {
    if let Some(Inline::Text(text, _)) = items.last_mut() {
        let len = text.trim_end_matches(' ').len();
        text.truncate(len);
        if text.is_empty() {
            items.pop();
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mark {
    Link(String),
    Strike,
    Underline,
    Highlight,
    Bold,
    Italic,
}

impl Mark {
    fn open(&self) -> &'static str {
        match self {
            Mark::Link(_) => "[",
            Mark::Strike => "~~",
            Mark::Underline => "~",
            Mark::Highlight => "==",
            Mark::Bold => "**",
            Mark::Italic => "*",
        }
    }

    fn close(&self) -> String {
        match self {
            Mark::Link(url) => format!("]({})", destination(url)),
            other => other.open().to_string(),
        }
    }
}

fn marks(fmt: &Fmt) -> Vec<Mark> {
    let mut marks: Vec<Mark> = fmt.link.iter().cloned().map(Mark::Link).collect();
    for (on, mark) in [
        (fmt.strike, Mark::Strike),
        (fmt.underline, Mark::Underline),
        (fmt.highlight, Mark::Highlight),
        (fmt.bold, Mark::Bold),
        (fmt.italic, Mark::Italic),
    ] {
        if on {
            marks.push(mark);
        }
    }
    marks
}

struct Emitter {
    out: String,
    /// A collapsed space waits to be written; it goes outside any marks that close first.
    pending_space: bool,
    open: Vec<Mark>,
    cell: bool,
}

impl Emitter {
    fn text(&mut self, text: &str, fmt: &Fmt) {
        let core = text.trim_matches(' ');
        if core.is_empty() {
            self.pending_space = true;
            return;
        }
        if text.starts_with(' ') {
            self.pending_space = true;
        }
        self.transition(marks(fmt));
        if fmt.code {
            self.out.push_str(&code_span(&if self.cell {
                core.replace('|', "\\|")
            } else {
                core.to_string()
            }));
        } else {
            self.out.push_str(&escape_inline(core, self.cell));
        }
        self.pending_space = text.ends_with(' ');
    }

    /// Closes the marks that no longer apply (and those opened after them), writes a pending
    /// space, then opens the wanted marks that are not open.
    fn transition(&mut self, wanted: Vec<Mark>) {
        let keep = self.open.iter().take_while(|m| wanted.contains(m)).count();
        for mark in self.open.drain(keep..).rev() {
            self.out.push_str(&mark.close());
        }
        if std::mem::take(&mut self.pending_space) {
            self.out.push(' ');
        }
        for mark in wanted {
            if !self.open.contains(&mark) {
                self.out.push_str(mark.open());
                self.open.push(mark);
            }
        }
    }

    fn finish(mut self) -> String {
        for mark in self.open.drain(..).rev() {
            self.out.push_str(&mark.close());
        }
        self.out
    }
}

/// Escapes characters that would start inline Markdown constructs.
pub(super) fn escape_inline(text: &str, cell: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        let previous = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let escape = match c {
            '\\' | '*' | '`' | '[' | ']' | '~' | '$' => true,
            '_' => {
                !(previous.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric))
            }
            '<' => next.is_some_and(|n| n.is_ascii_alphabetic() || matches!(n, '/' | '!' | '?')),
            '|' => cell,
            '=' => previous == Some('=') || next == Some('='),
            '&' => entity_like(&chars, i),
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Whether the `&` at `at` starts something that reads as a character reference (`&amp;`, `&#35;`).
fn entity_like(chars: &[char], at: usize) -> bool {
    let mut end = at + 1;
    if chars.get(end) == Some(&'#') {
        end += 1;
    }
    let start = end;
    while chars.get(end).is_some_and(char::is_ascii_alphanumeric) {
        end += 1;
    }
    end > start && chars.get(end) == Some(&';')
}

/// Escapes the first characters of every line that would start a block construct.
pub(super) fn escape_line_starts(text: &str) -> String {
    text.split('\n')
        .map(escape_line_start)
        .collect::<Vec<_>>()
        .join("\n")
}

fn escape_line_start(line: &str) -> String {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    let rest = &line[digits..];
    let ordered = digits > 0 && digits <= 9 && matches!(rest.chars().next(), Some('.' | ')'));
    let ordered = ordered && rest[1..].chars().next().is_none_or(|c| c == ' ');
    let hashes = line.chars().take_while(|&c| c == '#').count();
    let heading =
        (1..=6).contains(&hashes) && line[hashes..].chars().next().is_none_or(|c| c == ' ');
    let bullet = matches!(line.chars().next(), Some('-' | '+'))
        && line[1..].chars().next().is_none_or(|c| c == ' ');
    let rule = line.len() >= 3 && line.chars().all(|c| c == '-' || c == '=');
    if ordered {
        format!("{}\\{rest}", &line[..digits])
    } else if heading || bullet || rule || line.starts_with('>') {
        format!("\\{line}")
    } else {
        line.to_string()
    }
}

/// Link destination safe inside `(...)`.
pub(super) fn destination(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for c in url.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '<' => out.push_str("%3C"),
            '>' => out.push_str("%3E"),
            '\\' => out.push_str("%5C"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for byte in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(t: &str) -> Inline {
        Inline::Text(t.to_string(), Fmt::default())
    }

    fn styled(t: &str, f: impl FnOnce(&mut Fmt)) -> Inline {
        let mut fmt = Fmt::default();
        f(&mut fmt);
        Inline::Text(t.to_string(), fmt)
    }

    fn para(t: &str) -> Block {
        Block::Paragraph(t.to_string())
    }

    fn item(blocks: Vec<Block>) -> Item {
        Item { task: None, blocks }
    }

    fn bullets(items: Vec<Item>) -> Block {
        Block::List(List {
            ordered: false,
            start: 1,
            paren: false,
            items,
        })
    }

    #[test]
    fn escape_inline_table() {
        let table = [
            ("plain", false, "plain"),
            (
                "a*b_c`d[e]f~g$h\\i",
                false,
                "a\\*b_c\\`d\\[e\\]f\\~g\\$h\\\\i",
            ),
            ("snake_case _x_ a_", false, "snake_case \\_x\\_ a\\_"),
            (
                "1 < 2 <b> </i> <!-- <?",
                false,
                "1 < 2 \\<b> \\</i> \\<!-- \\<?",
            ),
            ("a|b", false, "a|b"),
            ("a|b", true, "a\\|b"),
            ("a==b = c", false, "a\\=\\=b = c"),
            (
                "&amp; &#35; AT&T R&D; x & y &",
                false,
                "\\&amp; \\&#35; AT&T R\\&D; x & y &",
            ),
            ("&", false, "&"),
        ];
        for (input, cell, expected) in table {
            assert_eq!(escape_inline(input, cell), expected, "{input}");
        }
    }

    #[test]
    fn escape_line_start_table() {
        let table = [
            ("1. x", "1\\. x"),
            ("12) x", "12\\) x"),
            ("1.5 kg", "1.5 kg"),
            ("2024", "2024"),
            ("1234567890. x", "1234567890. x"),
            ("# h", "\\# h"),
            ("###### h", "\\###### h"),
            ("####### h", "####### h"),
            ("#tag", "#tag"),
            ("#", "\\#"),
            ("- a", "\\- a"),
            ("+ a", "\\+ a"),
            ("-", "\\-"),
            ("-a", "-a"),
            ("---", "\\---"),
            ("===", "\\==="),
            ("--", "--"),
            ("> q", "\\> q"),
            ("text", "text"),
            ("", ""),
        ];
        for (input, expected) in table {
            assert_eq!(escape_line_start(input), expected, "{input}");
        }
        assert_eq!(escape_line_starts("a\\\n1. b\n# c"), "a\\\n1\\. b\n\\# c");
    }

    #[test]
    fn destination_table() {
        let table = [
            (
                "https://x.org/a b(c)<d>\\e",
                "https://x.org/a%20b%28c%29%3Cd%3E%5Ce",
            ),
            ("a\u{1}b\u{85}", "a%01b%C2%85"),
            ("https://x.org/ä", "https://x.org/ä"),
        ];
        for (input, expected) in table {
            assert_eq!(destination(input), expected);
        }
    }

    #[test]
    fn code_spans_and_fences_outgrow_backtick_runs() {
        assert_eq!(code_span("a"), "`a`");
        assert_eq!(code_span("a`b"), "``a`b``");
        assert_eq!(code_span("`a"), "`` `a ``");
        assert_eq!(code_span("a`"), "`` a` ``");
        assert_eq!(fenced("x", ""), "```\nx\n```");
        assert_eq!(fenced("a\n```\nb", "rs"), "````rs\na\n```\nb\n````");
    }

    #[test]
    fn spaces_collapse_across_items_and_trim_at_edges() {
        let items = [
            text(" a "),
            text(" b"),
            Inline::Break,
            text(" c "),
            Inline::Break,
            Inline::Break,
        ];
        assert_eq!(render_inline(&items, false), "a b\\\nc");
        assert_eq!(
            render_inline(&[Inline::Break, text("x"), text("  ")], false),
            "x"
        );
        assert_eq!(render_inline(&[text(" ")], false), "");
        assert_eq!(
            render_inline(&[text("a "), Inline::Break, Inline::Break, text("b")], true),
            "a<br><br>b"
        );
    }

    #[test]
    fn paragraphs_split_at_double_breaks() {
        let items = [
            text("a"),
            Inline::Break,
            text("b"),
            Inline::Break,
            Inline::Break,
            text("c"),
            Inline::Break,
        ];
        assert_eq!(render_paragraphs(&items, false), ["a\\\nb", "c"]);
        assert!(render_paragraphs(&[Inline::Break, text(" ")], false).is_empty());
    }

    #[test]
    fn marks_nest_minimally_and_keep_spaces_outside() {
        let bold = |t| styled(t, |f| f.bold = true);
        let italic = |t| styled(t, |f| f.italic = true);
        let both = |t| styled(t, |f| (f.bold, f.italic) = (true, true));
        let table = [
            (vec![bold("a "), text("b")], "**a** b"),
            (vec![text("a"), bold(" b ")], "a **b**"),
            (vec![bold("a"), both("b")], "**a*b***"),
            (vec![italic("a"), both("b")], "*a**b***"),
            (vec![both("a")], "***a***"),
            (vec![bold("a"), text(" "), bold("b")], "**a b**"),
            (vec![bold("a"), text(" "), italic("b")], "**a** *b*"),
            (
                vec![styled("x", |f| {
                    (f.strike, f.underline, f.highlight) = (true, true, true)
                })],
                "~~~==x==~~~",
            ),
        ];
        for (items, expected) in table {
            assert_eq!(render_inline(&items, false), expected);
        }
    }

    #[test]
    fn links_images_and_code_runs() {
        let link = |t: &str, url: &str| styled(t, |f| f.link = Some(url.to_string()));
        let items = [text("see "), link("a b", "https://x.org/p q"), text(" end")];
        assert_eq!(
            render_inline(&items, false),
            "see [a b](https://x.org/p%20q) end"
        );
        let image = Inline::Image {
            markdown: "![i](a.png)".to_string(),
            link: Some("u".to_string()),
        };
        assert_eq!(render_inline(&[image], false), "[![i](a.png)](u)");
        let image = Inline::Image {
            markdown: "![i](a.png)".to_string(),
            link: None,
        };
        assert_eq!(render_inline(&[text("x "), image], false), "x ![i](a.png)");
        let code = |t: &str| styled(t, |f| f.code = true);
        assert_eq!(render_inline(&[code("a|b`")], false), "`` a|b` ``");
        assert_eq!(render_inline(&[code("a|b")], true), "`a\\|b`");
    }

    #[test]
    fn block_rendering() {
        let row = |cells: [&str; 3]| cells.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        let table = Table {
            align: vec![Align::None, Align::Center, Align::Right],
            rows: vec![row(["a", "b", "c"]), row(["1", "", "3"])],
        };
        let blocks = [
            Block::Heading(2, "Title".to_string()),
            para("text"),
            Block::Code {
                lang: "rs".to_string(),
                text: "fn x() {}".to_string(),
            },
            Block::Quote(vec![para("q1"), para("q2")]),
            Block::Table(table),
            Block::Rule,
            Block::PageBreak,
        ];
        assert_eq!(
            render_document(&blocks),
            "## Title\n\ntext\n\n```rs\nfn x() {}\n```\n\n> q1\n>\n> q2\n\n| a | b | c |\n|---|:---:|---:|\n| 1 |  | 3 |\n\n---\n\n<!-- pagebreak -->\n"
        );
        assert_eq!(render_document(&[]), "");
    }

    #[test]
    fn lists_render_markers_tasks_nesting_and_looseness() {
        let nested = bullets(vec![item(vec![para("child")])]);
        let list = Block::List(List {
            ordered: true,
            start: 3,
            paren: true,
            items: vec![
                item(vec![para("one"), nested.clone()]),
                Item {
                    task: Some(true),
                    blocks: vec![para("two")],
                },
                Item {
                    task: Some(false),
                    blocks: vec![],
                },
            ],
        });
        assert_eq!(
            render_document(&[list]),
            "3) one\n   - child\n4) [x] two\n5) [ ]\n"
        );
        let loose = bullets(vec![
            item(vec![para("a"), para("b")]),
            item(vec![para("c")]),
        ]);
        assert_eq!(render_document(&[loose]), "- a\n\n  b\n\n- c\n");
        let code = bullets(vec![item(vec![
            Block::Code {
                lang: String::new(),
                text: "x".to_string(),
            },
            nested,
        ])]);
        assert_eq!(render_document(&[code]), "- ```\n  x\n  ```\n\n  - child\n");
    }

    #[test]
    fn adjacent_lists_alternate_markers() {
        let a = bullets(vec![item(vec![para("a")])]);
        let b = bullets(vec![item(vec![para("b")])]);
        let ordered = |n: &str, paren| {
            Block::List(List {
                ordered: true,
                start: 1,
                paren,
                items: vec![item(vec![para(n)])],
            })
        };
        assert_eq!(
            render_document(&[a.clone(), b.clone(), a.clone()]),
            "- a\n\n* b\n\n- a\n"
        );
        assert_eq!(
            render_document(&[ordered("x", false), ordered("y", false)]),
            "1. x\n\n1) y\n"
        );
        assert_eq!(
            render_document(&[ordered("x", true), ordered("y", true)]),
            "1) x\n\n1. y\n"
        );
        assert_eq!(
            render_document(&[ordered("x", false), ordered("y", true)]),
            "1. x\n\n1) y\n"
        );
        assert_eq!(
            render_document(&[a, ordered("x", false), b]),
            "- a\n\n1. x\n\n- b\n"
        );
    }
}
