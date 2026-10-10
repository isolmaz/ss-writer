//! Markdown emission shared by the `.doc` and `.odt` importers: inline merging and escaping,
//! blocks, list items, quotes, code blocks, footnote definitions and pipe tables.

use ssw_core::markdown::ast::HighlightColor;

/// Character formatting that Markdown can express.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Fmt {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    /// `~x~`
    pub underline: bool,
    /// `==x==` (yellow) or `=={color}x==`.
    pub highlight: Option<HighlightColor>,
    /// Monospace text, written as a code span.
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Inline {
    Text(String, Fmt),
    Break,
    /// Already-formatted Markdown (footnote references, images, placeholders).
    Raw(String),
    Link {
        url: String,
        kids: Vec<Inline>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Block {
    Heading(u8, String),
    Para(String),
    PageBreak,
    /// Horizontal rule.
    Rule,
    /// Rendered paragraphs of a block quote, separated by blank lines.
    Quote(String),
    /// Verbatim lines of a fenced code block.
    Code(String),
    Item {
        level: u8,
        marker: String,
        text: String,
    },
    /// Pipe table; `align` has one entry per column (missing entries mean no alignment).
    Table {
        rows: Vec<Vec<String>>,
        align: Vec<Align>,
    },
}

/// Horizontal alignment of a table column: `---`, `:--`, `:-:` or `--:`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Align {
    #[default]
    None,
    Left,
    Center,
    Right,
}

/// The alignment of each column of a table: the one most of the column's non-empty cells have,
/// over all rows (`aligns[row][column]` belongs to `cells[row][column]`). A tie, for instance a
/// centered header above left-aligned body cells, means no alignment.
pub(crate) fn majority_alignments(cells: &[Vec<String>], aligns: &[Vec<Align>]) -> Vec<Align> {
    const ALL: [Align; 4] = [Align::None, Align::Left, Align::Center, Align::Right];
    let columns = cells.iter().map(Vec::len).max().unwrap_or(0);
    (0..columns)
        .map(|column| {
            let mut votes = [0usize; 4];
            for (row, row_aligns) in cells.iter().zip(aligns) {
                if row.get(column).is_some_and(|text| !text.is_empty()) {
                    votes[row_aligns.get(column).copied().unwrap_or_default() as usize] += 1;
                }
            }
            let top = votes.iter().copied().max().unwrap_or(0);
            let mut best = ALL.iter().filter(|a| votes[**a as usize] == top);
            match (best.next(), best.next()) {
                (Some(&only), None) if top > 0 => only,
                _ => Align::None,
            }
        })
        .collect()
}

impl Align {
    fn delimiter(self) -> &'static str {
        match self {
            Align::None => "---",
            Align::Left => ":--",
            Align::Center => ":-:",
            Align::Right => "--:",
        }
    }
}

enum Atom {
    Text(String, Fmt),
    Break,
    Raw(String),
}

impl Atom {
    fn first_char(&self) -> Option<char> {
        match self {
            Atom::Text(s, _) | Atom::Raw(s) => s.chars().next(),
            Atom::Break => Some('\n'),
        }
    }
}

/// CommonMark punctuation for the flanking rules: everything that is neither a letter, a digit
/// nor white space (Unicode P and S categories included).
fn is_punct(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !c.is_control()
}

/// Splits a marked run into `(before, marked, after, delimiters_allowed)` so that its
/// delimiters are valid CommonMark emphasis. A closing run that follows punctuation must be
/// followed by white space or punctuation, and an opening run that precedes punctuation must
/// follow white space or punctuation; the boundary punctuation therefore moves outside the
/// delimiters when a letter or digit sits on the other side (`**學生—**證` becomes
/// `**學生**—證`). A run that is punctuation only, or a code span, which cannot give up its
/// backticks, is written without delimiters in that case.
///
/// `core` has no leading or trailing white space; `before` and `after` are the characters next
/// to it in the output (`None` or white space when no letter or digit touches the delimiters).
/// The DOCX, RTF and HTML importers use this same function.
pub(crate) fn flank(
    core: &str,
    code: bool,
    before: Option<char>,
    after: Option<char>,
) -> (&str, &str, &str, bool) {
    let word = |c: Option<char>| c.is_some_and(|c| !c.is_whitespace() && !is_punct(c));
    if code {
        return (
            &core[..0],
            core,
            &core[core.len()..],
            !word(before) && !word(after),
        );
    }
    let head = if word(before) {
        core.len() - core.trim_start_matches(is_punct).len()
    } else {
        0
    };
    let rest = &core[head..];
    let tail = if word(after) {
        rest.len() - rest.trim_end_matches(is_punct).len()
    } else {
        0
    };
    let cut = rest.len() - tail;
    (&core[..head], &rest[..cut], &rest[cut..], cut > 0)
}

/// Reads emphasis back from Markdown with the editor's parser, for the importer round-trip tests.
#[cfg(test)]
pub(crate) mod probe {
    use ssw_core::markdown::ast::{Inline, InlineKind};

    /// The flag letter of an emphasis node: `b` strong, `i` emphasis, `s` strike, `u` underline,
    /// `h` highlight.
    fn flag(kind: &InlineKind) -> Option<char> {
        match kind {
            InlineKind::Strong => Some('b'),
            InlineKind::Emphasis => Some('i'),
            InlineKind::Strikethrough => Some('s'),
            InlineKind::Underline => Some('u'),
            InlineKind::Highlight { .. } => Some('h'),
            _ => None,
        }
    }

    fn walk(src: &str, nodes: &[Inline], flags: &str, out: &mut Vec<(char, String)>) {
        for node in nodes {
            let text = match &node.kind {
                InlineKind::Text => node.range.get(src).unwrap_or("").to_string(),
                InlineKind::Escape => node.content.get(src).unwrap_or("").to_string(),
                InlineKind::Entity { decoded } => decoded.clone(),
                InlineKind::Code { text } => text.clone(),
                InlineKind::HardBreak | InlineKind::SoftBreak => "\n".to_string(),
                _ => String::new(),
            };
            let own = flags.to_string();
            out.extend(text.chars().map(|c| (c, own.clone())));
            let mut inner: Vec<char> = flags.chars().collect();
            inner.extend(flag(&node.kind));
            inner.sort_unstable();
            let inner: String = inner.into_iter().collect();
            walk(src, &node.children, &inner, out);
        }
    }

    /// The text of the first paragraph-like block of `md` grouped into runs of equal emphasis:
    /// `(text, flags)` with flags sorted alphabetically (`""` for plain text).
    pub(crate) fn runs(md: &str) -> Vec<(String, String)> {
        let document = ssw_core::markdown::parse_document(md);
        let mut chars = Vec::new();
        for block in &document.blocks {
            walk(md, &block.inlines, "", &mut chars);
        }
        let mut runs: Vec<(String, String)> = Vec::new();
        for (c, flags) in chars {
            match runs.last_mut() {
                Some((text, last)) if *last == flags => text.push(c),
                _ => runs.push((c.to_string(), flags)),
            }
        }
        runs
    }

    /// `runs` as owned pairs, for comparing with [`runs`].
    pub(crate) fn expect(runs: &[(&str, &str)]) -> Vec<(String, String)> {
        runs.iter()
            .map(|(text, flags)| ((*text).to_string(), (*flags).to_string()))
            .collect()
    }
}

/// Where inline text is going; decides how breaks and `|` are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ctx {
    Body,
    Heading,
    Cell,
}

pub(crate) fn escape_text(s: &str, ctx: Ctx) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        match c {
            '\\' | '`' | '*' | '[' | ']' | '<' | '>' | '~' | '$' => {
                out.push('\\');
                out.push(c);
            }
            '=' if prev == Some('=') || next == Some('=') => out.push_str("\\="),
            '|' if ctx == Ctx::Cell => out.push_str("\\|"),
            '_' => {
                let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
                if word(prev) && word(next) {
                    out.push('_');
                } else {
                    out.push_str("\\_");
                }
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

fn is_blank(a: &Atom) -> bool {
    match a {
        Atom::Break => true,
        Atom::Text(s, _) => s.trim().is_empty(),
        Atom::Raw(_) => false,
    }
}

fn atoms_of(inl: &[Inline], ctx: Ctx, out: &mut Vec<Atom>) {
    for i in inl {
        match i {
            Inline::Text(s, f) => match out.last_mut() {
                Some(Atom::Text(prev, pf)) if pf == f => prev.push_str(s),
                _ => out.push(Atom::Text(s.clone(), *f)),
            },
            Inline::Break => out.push(Atom::Break),
            Inline::Raw(r) => out.push(Atom::Raw(r.clone())),
            Inline::Link { url, kids } => {
                let inner = render_inlines(kids, ctx);
                let label = if inner.trim().is_empty() {
                    escape_text(url, ctx)
                } else {
                    inner
                };
                out.push(Atom::Raw(format!("[{label}]({})", md_dest(url))));
            }
        }
    }
}

/// Renders inline nodes to Markdown; leading and trailing blanks and breaks are trimmed.
pub(crate) fn render_inlines(inl: &[Inline], ctx: Ctx) -> String {
    let mut atoms = Vec::new();
    atoms_of(inl, ctx, &mut atoms);
    while atoms.last().is_some_and(is_blank) {
        atoms.pop();
    }
    let lead = atoms.iter().take_while(|a| is_blank(a)).count();
    atoms.drain(..lead);
    if let Some(Atom::Text(s, _)) = atoms.first_mut() {
        *s = s.trim_start().to_string();
    }
    if let Some(Atom::Text(s, _)) = atoms.last_mut() {
        *s = s.trim_end().to_string();
    }
    let mut out = String::new();
    for (i, a) in atoms.iter().enumerate() {
        match a {
            Atom::Break => out.push_str(match ctx {
                Ctx::Cell => "<br>",
                Ctx::Heading => " ",
                Ctx::Body => "\\\n",
            }),
            Atom::Raw(r) => out.push_str(r),
            Atom::Text(s, f) => {
                let core = s.trim();
                if core.is_empty() {
                    out.push_str(&escape_text(s, ctx));
                    continue;
                }
                let lead = &s[..s.len() - s.trim_start().len()];
                let trail = &s[s.trim_end().len()..];
                let formatted =
                    f.highlight.is_some() || f.strike || f.underline || f.bold || f.italic;
                let before = if lead.is_empty() {
                    out.chars().next_back()
                } else {
                    Some(' ')
                };
                let after = if trail.is_empty() {
                    atoms.get(i + 1).and_then(Atom::first_char)
                } else {
                    Some(' ')
                };
                let (pre, inner, post, marked) = if formatted {
                    flank(core, f.code, before, after)
                } else {
                    (&core[..0], core, &core[core.len()..], true)
                };
                let body = if f.code {
                    code_span(inner, ctx)
                } else {
                    escape_text(inner, ctx)
                };
                // outermost mark first
                let highlight = f.highlight.map(|c| {
                    if c == HighlightColor::Yellow {
                        "==".to_string()
                    } else {
                        format!("=={{{}}}", c.name())
                    }
                });
                let marks = [
                    (highlight, "=="),
                    (f.strike.then(|| "~~".to_string()), "~~"),
                    (f.underline.then(|| "~".to_string()), "~"),
                    (f.bold.then(|| "**".to_string()), "**"),
                    (f.italic.then(|| "*".to_string()), "*"),
                ];
                out.push_str(&escape_text(lead, ctx));
                out.push_str(&escape_text(pre, ctx));
                if marked {
                    for (open, _) in &marks {
                        out.push_str(open.as_deref().unwrap_or(""));
                    }
                }
                out.push_str(&body);
                if marked {
                    for (open, close) in marks.iter().rev() {
                        if open.is_some() {
                            out.push_str(close);
                        }
                    }
                }
                out.push_str(&escape_text(post, ctx));
                out.push_str(&escape_text(trail, ctx));
            }
        }
    }
    out
}

/// Escapes a paragraph start that would be read as a block construct.
pub(crate) fn escape_block_start(s: &str) -> String {
    if s.starts_with('#') || s.starts_with("- ") || s.starts_with("+ ") || s == "-" || s == "+" {
        return format!("\\{s}");
    }
    if !s.is_empty() && s.chars().all(|c| matches!(c, '-' | '=')) {
        return format!("\\{s}");
    }
    let digits = s.chars().take_while(char::is_ascii_digit).count();
    if (1..=9).contains(&digits) {
        let rest = &s[digits..];
        if rest.starts_with(". ") || rest.starts_with(") ") {
            return format!("{}\\{}", &s[..digits], rest);
        }
    }
    s.to_string()
}

pub(crate) fn join_cell(paras: Vec<String>) -> String {
    paras.join("<br>")
}

pub(crate) fn blocks_to_string(blocks: &[Block]) -> String {
    let mut out = String::new();
    let mut prev_item = false;
    for (i, b) in blocks.iter().enumerate() {
        let is_item = matches!(b, Block::Item { .. });
        if i > 0 {
            out.push_str(if prev_item && is_item { "\n" } else { "\n\n" });
        }
        prev_item = is_item;
        match b {
            Block::Heading(n, t) => {
                out.push_str(&"#".repeat(usize::from(*n)));
                out.push(' ');
                out.push_str(t);
            }
            Block::Para(t) => out.push_str(&escape_block_start(t)),
            Block::PageBreak => out.push_str("<!-- pagebreak -->"),
            Block::Rule => out.push_str("---"),
            Block::Quote(text) => {
                let lines: Vec<String> = text
                    .split('\n')
                    .map(|l| {
                        if l.is_empty() {
                            ">".to_string()
                        } else {
                            format!("> {l}")
                        }
                    })
                    .collect();
                out.push_str(&lines.join("\n"));
            }
            Block::Code(text) => {
                let fence = "`".repeat(longest_backtick_run(text).max(2) + 1);
                out.push_str(&fence);
                out.push('\n');
                out.push_str(text.trim_end_matches('\n'));
                out.push('\n');
                out.push_str(&fence);
            }
            Block::Item {
                level,
                marker,
                text,
            } => {
                let indent = "    ".repeat(usize::from(*level));
                let cont = format!("{indent}{}", " ".repeat(marker.len() + 1));
                out.push_str(&indent);
                out.push_str(marker);
                out.push(' ');
                for (n, line) in text.split('\n').enumerate() {
                    if n > 0 {
                        out.push('\n');
                        if !line.is_empty() {
                            out.push_str(&cont);
                        }
                    }
                    out.push_str(line);
                }
            }
            Block::Table { rows, align } => out.push_str(&render_table(rows, align)),
        }
    }
    out
}

fn longest_backtick_run(text: &str) -> usize {
    let (mut longest, mut run) = (0, 0);
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// An inline code span; the fence is longer than any backtick run inside.
fn code_span(text: &str, ctx: Ctx) -> String {
    let text: String = text
        .chars()
        .map(|c| if matches!(c, '\n' | '\r') { ' ' } else { c })
        .collect();
    let text = if ctx == Ctx::Cell {
        text.replace('|', "\\|")
    } else {
        text
    };
    let fence = "`".repeat(longest_backtick_run(&text) + 1);
    if text.starts_with(['`', ' ']) || text.ends_with(['`', ' ']) {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// A link destination, in angle brackets when it contains characters that end a bare one.
pub(crate) fn md_dest(dest: &str) -> String {
    if !dest.is_empty() && !dest.contains([' ', '(', ')', '<', '>', '\n', '\t']) {
        return dest.to_string();
    }
    let encoded: String = dest
        .chars()
        .map(|c| match c {
            '<' => "%3C".to_string(),
            '>' => "%3E".to_string(),
            '\n' | '\t' => "%20".to_string(),
            other => other.to_string(),
        })
        .collect();
    format!("<{encoded}>")
}

/// `![alt](dest)` or, with a display width in pixels, `![alt|width](dest)`.
pub(crate) fn image_markdown(alt: &str, dest: &str, width: Option<u32>) -> String {
    let alt = alt
        .replace(['\n', '\r'], " ")
        .replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('|', "\\|");
    match width {
        Some(w) => format!("![{alt}|{w}]({})", md_dest(dest)),
        None => format!("![{alt}]({})", md_dest(dest)),
    }
}

/// `[^label]: body` with continuation lines indented by four spaces, ending in a newline.
pub(crate) fn footnote_definition(label: &str, body: &str) -> String {
    let mut lines = body.trim().lines();
    let first = lines.next().unwrap_or("");
    let mut out = format!("[^{label}]: {first}").trim_end().to_string();
    out.push('\n');
    for line in lines {
        if !line.is_empty() {
            out.push_str("    ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

pub(crate) fn render_table(rows: &[Vec<String>], align: &[Align]) -> String {
    let ncols = rows.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        out.push('|');
        for c in 0..ncols {
            out.push(' ');
            out.push_str(r.get(c).map_or("", String::as_str));
            out.push_str(" |");
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            for c in 0..ncols {
                out.push(' ');
                out.push_str(align.get(c).copied().unwrap_or_default().delimiter());
                out.push_str(" |");
            }
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Inline {
        Inline::Text(s.into(), Fmt::default())
    }

    fn bold(s: &str) -> Inline {
        Inline::Text(
            s.into(),
            Fmt {
                bold: true,
                ..Fmt::default()
            },
        )
    }

    #[test]
    fn escapes_markdown_syntax() {
        let cases: [(&str, Ctx, &str); 10] = [
            ("a*b_c_d [x]", Ctx::Body, "a\\*b_c_d \\[x\\]"),
            ("a|b", Ctx::Cell, "a\\|b"),
            ("a|b", Ctx::Body, "a|b"),
            (
                "snake_ _lead trail_",
                Ctx::Body,
                "snake\\_ \\_lead trail\\_",
            ),
            ("1 == 2 = 3", Ctx::Body, "1 \\=\\= 2 = 3"),
            (
                "cost $5 `x` <y> ~z~",
                Ctx::Body,
                "cost \\$5 \\`x\\` \\<y\\> \\~z\\~",
            ),
            ("two\nlines\r!", Ctx::Body, "two lines !"),
            ("back\\slash", Ctx::Body, "back\\\\slash"),
            ("", Ctx::Body, ""),
            ("plain", Ctx::Heading, "plain"),
        ];
        for (input, ctx, want) in cases {
            assert_eq!(escape_text(input, ctx), want, "{input:?}");
        }
    }

    #[test]
    fn block_start_escapes() {
        let cases: [(&str, &str); 11] = [
            ("# t", "\\# t"),
            ("- x", "\\- x"),
            ("+ x", "\\+ x"),
            ("-", "\\-"),
            ("+", "\\+"),
            ("---", "\\---"),
            ("===", "\\==="),
            ("1. foo", "1\\. foo"),
            ("12) foo", "12\\) foo"),
            ("1.5 apples", "1.5 apples"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(escape_block_start(input), want, "{input:?}");
        }
        assert_eq!(
            escape_block_start("1234567890. x"),
            "1234567890. x",
            "ten digits is not a list marker"
        );
    }

    #[test]
    fn inline_formatting_merges_and_trims() {
        let f = |b, i, s| Fmt {
            bold: b,
            italic: i,
            strike: s,
            ..Fmt::default()
        };
        let inl = vec![
            Inline::Break,
            Inline::Text("  ".into(), Fmt::default()),
            Inline::Text("Hello ".into(), f(true, false, false)),
            Inline::Text("world".into(), f(true, false, false)),
            t(" and "),
            Inline::Text("both".into(), f(true, true, false)),
            Inline::Text(" gone ".into(), f(false, false, true)),
            Inline::Break,
            Inline::Raw("![i](x.png)".into()),
            Inline::Break,
            t("  "),
        ];
        assert_eq!(
            render_inlines(&inl, Ctx::Body),
            "**Hello world** and ***both*** ~~gone~~ \\\n![i](x.png)"
        );
        assert_eq!(
            render_inlines(&inl, Ctx::Cell),
            "**Hello world** and ***both*** ~~gone~~ <br>![i](x.png)"
        );
        assert_eq!(
            render_inlines(&[t("a"), Inline::Break, t("b")], Ctx::Heading),
            "a b"
        );
        assert_eq!(render_inlines(&[], Ctx::Body), "");
        // whitespace-only formatted runs keep their spacing without markers
        assert_eq!(
            render_inlines(
                &[
                    t("a"),
                    Inline::Text(" ".into(), f(true, false, false)),
                    t("b")
                ],
                Ctx::Body
            ),
            "a b"
        );
    }

    #[test]
    fn links_use_text_or_url_as_label() {
        let link = Inline::Link {
            url: "http://x.org/a_b".into(),
            kids: vec![bold("see"), t(" this")],
        };
        assert_eq!(
            render_inlines(&[t("go "), link], Ctx::Body),
            "go [**see** this](http://x.org/a_b)"
        );
        let empty = Inline::Link {
            url: "http://x.org/a_b".into(),
            kids: vec![t("  ")],
        };
        assert_eq!(
            render_inlines(&[empty], Ctx::Body),
            "[http://x.org/a_b](http://x.org/a_b)"
        );
    }

    #[test]
    fn renders_blocks() {
        let blocks = vec![
            Block::Heading(2, "Title".into()),
            Block::Para("1. not a list".into()),
            Block::Item {
                level: 0,
                marker: "-".into(),
                text: "one\ncont".into(),
            },
            Block::Item {
                level: 1,
                marker: "1.".into(),
                text: "nested".into(),
            },
            Block::PageBreak,
            Block::Table {
                rows: vec![vec!["a".into(), "b".into()], vec!["c".into()]],
                align: Vec::new(),
            },
        ];
        assert_eq!(
            blocks_to_string(&blocks),
            "## Title\n\n1\\. not a list\n\n- one\n  cont\n    1. nested\n\n<!-- pagebreak -->\n\n| a | b |\n| --- | --- |\n| c |  |"
        );
        assert_eq!(blocks_to_string(&[]), "");
        assert_eq!(render_table(&[], &[]), "");
        let rows = [vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ]];
        assert_eq!(
            render_table(&rows, &[Align::Left, Align::Center, Align::Right]),
            "| a | b | c | d |\n| :-- | :-: | --: | --- |"
        );
        assert_eq!(join_cell(vec!["a".into(), "b".into()]), "a<br>b");
    }

    #[test]
    fn underline_highlight_and_code_marks() {
        let fmt = |f: Fmt, s: &str| render_inlines(&[Inline::Text(s.into(), f)], Ctx::Body);
        let base = Fmt::default();
        assert_eq!(
            fmt(
                Fmt {
                    underline: true,
                    ..base
                },
                "hi"
            ),
            "~hi~"
        );
        assert_eq!(
            fmt(
                Fmt {
                    highlight: Some(HighlightColor::Yellow),
                    ..base
                },
                "x"
            ),
            "==x=="
        );
        assert_eq!(
            fmt(
                Fmt {
                    highlight: Some(HighlightColor::Blue),
                    ..base
                },
                " x "
            ),
            "=={blue}x=="
        );
        let all = Fmt {
            bold: true,
            italic: true,
            strike: true,
            underline: true,
            highlight: Some(HighlightColor::Red),
            code: false,
        };
        assert_eq!(fmt(all, "x"), "=={red}~~~***x***~~~==");
        assert_eq!(fmt(Fmt { code: true, ..base }, "a`b"), "``a`b``");
        assert_eq!(fmt(Fmt { code: true, ..base }, "`x"), "`` `x ``");
        assert_eq!(
            fmt(
                Fmt {
                    code: true,
                    bold: true,
                    ..base
                },
                "a*b"
            ),
            "**`a*b`**"
        );
        assert_eq!(fmt(Fmt { code: true, ..base }, " x "), "`x`");
        let inner = [
            t("a"),
            Inline::Text(" x ".into(), Fmt { code: true, ..base }),
            t("b"),
        ];
        assert_eq!(render_inlines(&inner, Ctx::Body), "a `x` b");
        assert_eq!(fmt(Fmt { code: true, ..base }, "a\nb"), "`a b`");
        let cell = render_inlines(
            &[Inline::Text("a|b".into(), Fmt { code: true, ..base })],
            Ctx::Cell,
        );
        assert_eq!(cell, "`a\\|b`");
    }

    /// The text of every strong node CommonMark finds in `md`.
    fn strong_texts(md: &str) -> Vec<String> {
        use ssw_core::markdown::ast::{Inline as Node, InlineKind};
        fn walk(src: &str, nodes: &[Node], out: &mut Vec<String>) {
            for node in nodes {
                if matches!(node.kind, InlineKind::Strong) {
                    out.push(node.content.get(src).unwrap_or("").to_string());
                }
                walk(src, &node.children, out);
            }
        }
        let document = ssw_core::markdown::parse_document(md);
        let mut out = Vec::new();
        for block in &document.blocks {
            walk(md, &block.inlines, &mut out);
        }
        out
    }

    #[test]
    fn delimiters_are_always_valid_commonmark() {
        let strong = |s: &str| {
            Inline::Text(
                s.into(),
                Fmt {
                    bold: true,
                    ..Fmt::default()
                },
            )
        };
        let code_strong = |s: &str| {
            Inline::Text(
                s.into(),
                Fmt {
                    bold: true,
                    code: true,
                    ..Fmt::default()
                },
            )
        };
        let cases: [(Vec<Inline>, &str, Vec<&str>); 10] = [
            // a closing run after punctuation must not touch a letter: the dash moves out
            (
                vec![strong("\u{570b}\u{7acb}\u{2014}"), t("\u{5b78}\u{751f}")],
                "**\u{570b}\u{7acb}**\u{2014}\u{5b78}\u{751f}",
                vec!["\u{570b}\u{7acb}"],
            ),
            // and an opening run before punctuation must not follow a letter
            (
                vec![t("\u{5b78}\u{751f}"), strong("\u{2014}\u{570b}\u{7acb}")],
                "\u{5b78}\u{751f}\u{2014}**\u{570b}\u{7acb}**",
                vec!["\u{570b}\u{7acb}"],
            ),
            (vec![t("a"), strong("(x)"), t("b")], "a(**x**)b", vec!["x"]),
            // only punctuation: no delimiters
            (
                vec![t("a"), strong("\u{2014}"), t("b")],
                "a\u{2014}b",
                vec![],
            ),
            (vec![strong("\u{2014}"), t("b")], "\u{2014}b", vec![]),
            // a code span cannot give up its backticks
            (vec![t("a"), code_strong("x"), t("b")], "a`x`b", vec![]),
            (
                vec![t("a "), code_strong("x"), t(" b")],
                "a **`x`** b",
                vec!["`x`"],
            ),
            // punctuation next to white space, punctuation or the edges is fine as written
            (vec![t("("), strong("x"), t(")")], "(**x**)", vec!["x"]),
            (
                vec![t("a "), strong("!x?"), t(" b")],
                "a **!x?** b",
                vec!["!x?"],
            ),
            (
                vec![strong("\u{2014}x\u{2014}")],
                "**\u{2014}x\u{2014}**",
                vec!["\u{2014}x\u{2014}"],
            ),
        ];
        for (inlines, want, strongs) in cases {
            let md = render_inlines(&inlines, Ctx::Body);
            assert_eq!(md, want);
            assert_eq!(strong_texts(&md), strongs, "{md}");
        }
    }

    #[test]
    fn columns_take_the_majority_alignment() {
        use Align::{Center, Left, None as Plain, Right};
        let cells: Vec<Vec<String>> = [
            ["h", "h", "h", "h", ""],
            ["a", "b", "c", "d", ""],
            ["a", "", "c", "d", ""],
        ]
        .iter()
        .map(|row| row.iter().map(|s| (*s).to_string()).collect())
        .collect();
        let aligns = [
            vec![Center, Center, Right, Left, Right],
            vec![Center, Plain, Right, Left, Right],
            vec![Center, Left, Plain, Plain, Right],
        ];
        // column 1: center x3; 2: center, none (an empty cell does not vote); 3: right, right, none;
        // 4: left, left, none; 5: only empty cells
        assert_eq!(
            majority_alignments(&cells, &aligns),
            [Center, Plain, Right, Left, Plain]
        );
        assert_eq!(majority_alignments(&cells, &[]), [Plain; 5]);
        assert!(majority_alignments(&[], &[]).is_empty());
    }

    #[test]
    fn quote_code_and_rule_blocks() {
        let blocks = vec![
            Block::Quote("a\n\nb".into()),
            Block::Code("let x = 1;\n``` fence\n".into()),
            Block::Rule,
            Block::Code("x".into()),
            Block::Item {
                level: 0,
                marker: "-".into(),
                text: "one\n\ntwo".into(),
            },
        ];
        assert_eq!(
            blocks_to_string(&blocks),
            "> a\n>\n> b\n\n````\nlet x = 1;\n``` fence\n````\n\n---\n\n```\nx\n```\n\n- one\n\n  two"
        );
    }

    #[test]
    fn destinations_images_and_footnotes() {
        let dests = [
            ("http://x/a", "http://x/a"),
            ("a b", "<a b>"),
            ("a(b)", "<a(b)>"),
            ("", "<>"),
            ("x<y>\tz", "<x%3Cy%3E%20z>"),
            ("a\nb", "<a%20b>"),
        ];
        for (dest, want) in dests {
            assert_eq!(md_dest(dest), want, "{dest:?}");
        }
        assert_eq!(
            image_markdown("a [b]\\|\nc", "p q.png", Some(30)),
            "![a \\[b\\]\\\\\\| c|30](<p q.png>)"
        );
        assert_eq!(image_markdown("x", "i.png", None), "![x](i.png)");
        assert_eq!(
            footnote_definition("1", " first  \nsecond\n\nthird \n"),
            "[^1]: first\n    second\n\n    third\n"
        );
        assert_eq!(footnote_definition("2", ""), "[^2]:\n");
        let link = Inline::Link {
            url: "a b".into(),
            kids: vec![t("x")],
        };
        assert_eq!(render_inlines(&[link], Ctx::Body), "[x](<a b>)");
    }
}
