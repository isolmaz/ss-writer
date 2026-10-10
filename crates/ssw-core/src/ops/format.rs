//! Inline formatting: toggling emphasis-like styles, links, images and the date.

use std::borrow::Cow;

use crate::edit::{Assoc, Edit, Selection, Transaction};
use crate::markdown::ast::{
    self, BlockKind, HighlightColor, Inline, InlineKind, LinkKind, NodeRef, iter_inlines,
};
use crate::text::TextRange;

use super::{Ctx, list_marker_len};

/// An inline style that [`toggle_inline`] can apply or remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlineStyle {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    /// `==x==`, or `=={color}x==`.
    Highlight(Option<HighlightColor>),
    Code,
    Math,
}

impl InlineStyle {
    /// True when `kind` is a node of this style (a highlight of any color matches any
    /// highlight style).
    fn matches(self, kind: &InlineKind) -> bool {
        matches!(
            (self, kind),
            (InlineStyle::Bold, InlineKind::Strong)
                | (InlineStyle::Italic, InlineKind::Emphasis)
                | (InlineStyle::Underline, InlineKind::Underline)
                | (InlineStyle::Strikethrough, InlineKind::Strikethrough)
                | (InlineStyle::Highlight(_), InlineKind::Highlight { .. })
                | (InlineStyle::Code, InlineKind::Code { .. })
                | (InlineStyle::Math, InlineKind::InlineMath)
        )
    }

    /// Opening and closing marker for wrapping `content`.
    fn markers(self, content: &str) -> (String, String) {
        let pair = |m: &str| (m.to_owned(), m.to_owned());
        match self {
            InlineStyle::Bold => pair("**"),
            InlineStyle::Italic => pair("*"),
            InlineStyle::Underline => pair("~"),
            InlineStyle::Strikethrough => pair("~~"),
            InlineStyle::Highlight(color) => (highlight_open(color), "==".to_owned()),
            InlineStyle::Math => pair("$"),
            InlineStyle::Code => {
                let fence = "`".repeat(longest_backtick_run(content) + 1);
                let pad = if content.starts_with('`') || content.ends_with('`') {
                    " "
                } else {
                    ""
                };
                (format!("{fence}{pad}"), format!("{pad}{fence}"))
            }
        }
    }
}

fn highlight_open(color: Option<HighlightColor>) -> String {
    match color {
        Some(c) => format!("=={{{}}}", c.name()),
        None => "==".to_owned(),
    }
}

/// Length of the longest run of backticks in `text` (0 without any).
pub(crate) fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`')
        .map(str::len)
        .max()
        .unwrap_or_default()
}

/// Inline nodes in the tree path at `offset`, outermost first.
fn inline_chain(ast: &ast::Document, offset: usize) -> Vec<&Inline> {
    ast.path_at(offset)
        .into_iter()
        .filter_map(NodeRef::as_inline)
        .collect()
}

/// The innermost node of `style` whose range contains `range` (inclusive edges).
fn styled_node_at(ast: &ast::Document, style: InlineStyle, range: TextRange) -> Option<&Inline> {
    let mut probes = vec![range.start];
    if range.start > 0 {
        // A caret right after an element touches it although the tree path picks the next
        // sibling there.
        probes.push(range.start - 1);
    }
    probes.into_iter().find_map(|probe| {
        inline_chain(ast, probe)
            .into_iter()
            .rev()
            .find(|node| style.matches(&node.kind) && node.range.contains_range(range))
    })
}

/// Nodes whose source is a unit: markers inside them are literal or part of a bigger syntax.
fn is_atomic(node: &Inline) -> bool {
    match &node.kind {
        InlineKind::Code { .. }
        | InlineKind::InlineMath
        | InlineKind::Image(_)
        | InlineKind::WikiLink(_)
        | InlineKind::FootnoteRef { .. }
        | InlineKind::RawHtml
        | InlineKind::Entity { .. }
        | InlineKind::Escape
        | InlineKind::HardBreak
        | InlineKind::SoftBreak => true,
        InlineKind::Link(link) => matches!(link.kind, LinkKind::Autolink { .. }),
        _ => false,
    }
}

/// Moves `offset` out of any node it would cut apart: atomic nodes, the middle of a marker, and
/// nodes that `selection` only partly covers. The start moves to the node start, the end to
/// the node end.
fn snap_endpoint(
    ast: &ast::Document,
    offset: usize,
    is_start: bool,
    selection: TextRange,
) -> usize {
    let mut p = offset;
    for node in inline_chain(ast, offset).into_iter().rev() {
        let strictly_inside = node.range.start < p && p < node.range.end;
        if !strictly_inside || matches!(node.kind, InlineKind::Text) {
            continue;
        }
        let in_marker = node.markers.iter().any(|m| m.start < p && p < m.end);
        if is_atomic(node) || in_marker || !node.range.contains_range(selection) {
            p = if is_start {
                node.range.start
            } else {
                node.range.end
            };
        }
    }
    p
}

/// A run of inline content: a paragraph, heading or table cell.
struct Region<'a> {
    inlines: &'a [Inline],
    span: TextRange,
}

fn regions(ast: &ast::Document) -> Vec<Region<'_>> {
    let mut out = Vec::new();
    for block in ast.iter_blocks() {
        match &block.kind {
            BlockKind::Table(table) => {
                for row in std::iter::once(&table.header).chain(&table.rows) {
                    for cell in &row.cells {
                        if let (Some(first), Some(last)) =
                            (cell.inlines.first(), cell.inlines.last())
                        {
                            out.push(Region {
                                inlines: &cell.inlines,
                                span: TextRange::new(first.range.start, last.range.end),
                            });
                        }
                    }
                }
            }
            BlockKind::CodeBlock(_)
            | BlockKind::MathBlock(_)
            | BlockKind::FrontMatter { .. }
            | BlockKind::HtmlBlock(_) => {}
            _ => {
                if let (Some(first), Some(last)) = (block.inlines.first(), block.inlines.last()) {
                    out.push(Region {
                        inlines: &block.inlines,
                        span: TextRange::new(first.range.start, last.range.end),
                    });
                }
            }
        }
    }
    out
}

/// Styled nodes that together make up all of `segment` (whitespace between them allowed);
/// `None` when part of the segment is not styled.
fn covering_nodes<'a>(
    ctx: Ctx<'a>,
    region: &Region<'a>,
    style: InlineStyle,
    segment: TextRange,
) -> Option<Vec<&'a Inline>> {
    if let Some(node) = styled_node_at(ctx.ast, style, segment) {
        return Some(vec![node]);
    }
    let mut nodes: Vec<&Inline> = Vec::new();
    for node in iter_inlines(region.inlines) {
        let inside = segment.contains_range(node.range);
        let nested = nodes
            .last()
            .is_some_and(|n| n.range.contains_range(node.range));
        if inside && !nested && style.matches(&node.kind) {
            nodes.push(node);
        }
    }
    if nodes.is_empty() {
        return None;
    }
    let mut cursor = segment.start;
    for node in &nodes {
        if !ctx
            .slice(TextRange::new(cursor, node.range.start))
            .trim()
            .is_empty()
        {
            return None;
        }
        cursor = node.range.end;
    }
    ctx.slice(TextRange::new(cursor, segment.end))
        .trim()
        .is_empty()
        .then_some(nodes)
}

/// Toggles `style` on the selection, or at the caret: an empty selection unwraps the element
/// holding the caret or inserts an empty pair with the caret inside. `None` where inline
/// formatting does not apply (code, math, HTML, front matter).
pub fn toggle_inline(ctx: Ctx<'_>, style: InlineStyle) -> Option<Transaction> {
    if ctx.selection.is_caret() {
        toggle_at_caret(ctx, style)
    } else {
        toggle_over_selection(ctx, style)
    }
}

/// Whether the caret (or the whole selection) is inside an element of `style`.
pub fn is_style_active(ctx: Ctx<'_>, style: InlineStyle) -> bool {
    styled_node_at(ctx.ast, style, ctx.selection.range()).is_some()
}

fn toggle_at_caret(ctx: Ctx<'_>, style: InlineStyle) -> Option<Transaction> {
    let caret = ctx.selection.head;
    if let Some(node) = styled_node_at(ctx.ast, style, TextRange::at(caret)) {
        return Some(unwrap_or_recolor(ctx, style, &[node]));
    }
    if ctx.in_verbatim_block(caret) {
        return None;
    }
    let at = snap_endpoint(ctx.ast, caret, false, TextRange::at(caret));
    let (open, close) = style.markers("");
    let inside = at + open.len();
    let edit = Edit::insert(at, format!("{open}{close}"));
    Some(ctx.transaction(vec![edit], Selection::caret(inside)))
}

fn toggle_over_selection(ctx: Ctx<'_>, style: InlineStyle) -> Option<Transaction> {
    let selection = ctx.selection.range();
    let all_regions = regions(ctx.ast);
    let mut segments: Vec<(TextRange, &Region<'_>)> = Vec::new();
    for region in &all_regions {
        let Some(part) = selection.intersection(region.span) else {
            continue;
        };
        let text = ctx.slice(part);
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        if lead + trail >= text.len() {
            continue;
        }
        let trimmed = TextRange::new(part.start + lead, part.end - trail);
        let start = snap_endpoint(ctx.ast, trimmed.start, true, trimmed);
        let end = snap_endpoint(ctx.ast, trimmed.end, false, trimmed);
        segments.push((TextRange::new(start, end), region));
    }
    if segments.is_empty() {
        return None;
    }
    let mut covering: Vec<&Inline> = Vec::new();
    let mut all_covered = true;
    for (segment, region) in &segments {
        match covering_nodes(ctx, region, style, *segment) {
            Some(nodes) => covering.extend(nodes),
            None => all_covered = false,
        }
    }
    if all_covered {
        return Some(unwrap_or_recolor(ctx, style, &covering));
    }
    let ranges: Vec<TextRange> = segments.iter().map(|(s, _)| *s).collect();
    Some(wrap_segments(ctx, style, &ranges, &all_regions))
}

/// Removes the markers of `nodes`, or changes their highlight color when `style` asks for a
/// different one.
fn unwrap_or_recolor(ctx: Ctx<'_>, style: InlineStyle, nodes: &[&Inline]) -> Transaction {
    let mut edits = Vec::new();
    if let InlineStyle::Highlight(color) = style {
        let color_of = |n: &Inline| n.kind.highlight_color();
        if nodes.iter().any(|n| color_of(n) != color) {
            for node in nodes.iter().filter(|n| color_of(n) != color) {
                edits.push(Edit::replace(node.markers[0], highlight_open(color)));
            }
            return ctx.transaction_mapped(edits);
        }
    }
    for node in nodes {
        edits.extend(node.markers.iter().map(|m| Edit::delete(*m)));
    }
    ctx.transaction_mapped(edits)
}

/// Wraps every segment in the style markers, first removing markers of the same style that are
/// now redundant inside a segment.
fn wrap_segments(
    ctx: Ctx<'_>,
    style: InlineStyle,
    segments: &[TextRange],
    regions: &[Region<'_>],
) -> Transaction {
    let mut edits = Vec::new();
    for segment in segments {
        let (open, close) = style.markers(&ctx.slice(*segment));
        edits.push(Edit::insert(segment.start, open));
        edits.push(Edit::insert(segment.end, close));
        let mut last_removed: Option<TextRange> = None;
        for node in regions.iter().flat_map(|r| iter_inlines(r.inlines)) {
            let nested = last_removed.is_some_and(|r| r.contains_range(node.range));
            if segment.contains_range(node.range) && style.matches(&node.kind) && !nested {
                edits.extend(node.markers.iter().map(|m| Edit::delete(*m)));
                last_removed = Some(node.range);
            }
        }
    }
    edits.sort_by_key(|e| (e.range.start, e.range.end));
    let draft = Transaction::new(edits, ctx.selection, ctx.selection);
    let first = segments[0];
    let last = segments[segments.len() - 1];
    let start = draft.map_offset(first.start, Assoc::After);
    let end = draft.map_offset(last.end, Assoc::Before);
    let after = if ctx.selection.is_reversed() {
        Selection::new(end, start)
    } else {
        Selection::new(start, end)
    };
    Transaction {
        selection_after: after,
        ..draft
    }
}

/// An inline link `[text](url "title")` found at the selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    /// The whole link.
    pub range: TextRange,
    /// Link text as written.
    pub text: String,
    /// Resolved destination.
    pub url: String,
    pub title: Option<String>,
}

/// The inline link under the caret (or containing the selection), for the edit popup.
pub fn link_at(ctx: Ctx<'_>) -> Option<LinkInfo> {
    let range = ctx.selection.range();
    let mut probes = vec![range.start];
    if range.start > 0 {
        probes.push(range.start - 1);
    }
    probes.into_iter().find_map(|probe| {
        inline_chain(ctx.ast, probe)
            .into_iter()
            .rev()
            .find_map(|node| {
                let InlineKind::Link(link) = &node.kind else {
                    return None;
                };
                let is_inline = matches!(link.kind, LinkKind::Inline { .. });
                (is_inline && node.range.contains_range(range)).then(|| LinkInfo {
                    range: node.range,
                    text: ctx.slice(node.content).into_owned(),
                    url: link.dest.clone(),
                    title: link.title.clone(),
                })
            })
    })
}

/// Edits the link at the selection to `[text](url)` (keeping its title), or replaces the
/// selection with a new link. The caret ends after the link.
pub fn set_link(ctx: Ctx<'_>, text: &str, url: &str) -> Transaction {
    let existing = link_at(ctx);
    let range = existing
        .as_ref()
        .map_or(ctx.selection.range(), |info| info.range);
    let title = existing
        .and_then(|info| info.title)
        .map(|t| format!(" \"{}\"", t.replace('\\', "\\\\").replace('"', "\\\"")))
        .unwrap_or_default();
    let link = format!(
        "[{}]({}{title})",
        escape_brackets(text),
        link_destination(url)
    );
    let caret = range.start + link.len();
    ctx.transaction(vec![Edit::replace(range, link)], Selection::caret(caret))
}

/// Replaces the selection with `![alt](dest)`; the caret ends after the image.
pub fn insert_image(ctx: Ctx<'_>, alt: &str, dest: &str) -> Transaction {
    let image = format!("![{}]({})", escape_brackets(alt), link_destination(dest));
    let range = ctx.selection.range();
    let caret = range.start + image.len();
    ctx.transaction(vec![Edit::replace(range, image)], Selection::caret(caret))
}

/// Escapes `[` and `]` with backslashes when they are not balanced in `text`.
pub fn escape_brackets(text: &str) -> Cow<'_, str> {
    let mut depth = 0i32;
    let mut balanced = true;
    for ch in text.chars() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                balanced &= depth >= 0;
            }
            _ => {}
        }
    }
    if balanced && depth == 0 {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.replace('[', "\\[").replace(']', "\\]"))
}

/// A link destination: bare when it is safe, otherwise in angle brackets.
pub fn link_destination(url: &str) -> Cow<'_, str> {
    let plain = !url.is_empty()
        && !url
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>'));
    if plain {
        return Cow::Borrowed(url);
    }
    let escaped = url
        .replace('\\', "\\\\")
        .replace('<', "\\<")
        .replace('>', "\\>");
    Cow::Owned(format!("<{escaped}>"))
}

/// Replaces the selection with the already formatted date. A date that would start an ordered
/// list at its position (`1. March 2026`) gets its delimiter escaped (`1\. March 2026`).
pub fn insert_date(ctx: Ctx<'_>, formatted: &str) -> Transaction {
    let range = ctx.selection.range();
    let line = ctx.line_of(range.start);
    let starts_block = ctx.content_start(line) == range.start;
    let text = match list_marker_len(formatted) {
        Some(len) if starts_block && formatted.as_bytes()[0].is_ascii_digit() => {
            let (number, rest) = formatted.split_at(len - 1);
            format!("{number}\\{rest}")
        }
        _ => formatted.to_owned(),
    };
    let caret = range.start + text.len();
    ctx.transaction(vec![Edit::replace(range, text)], Selection::caret(caret))
}

#[cfg(test)]
mod tests {
    use super::super::testing::*;
    use super::*;
    use crate::markdown::parse_document;
    use crate::text::Buffer;

    fn style(s: InlineStyle) -> impl FnOnce(Ctx<'_>) -> Option<Transaction> {
        move |c| toggle_inline(c, s)
    }

    fn ctx_with<T>(marked: &str, f: impl FnOnce(Ctx<'_>) -> T) -> T {
        let (text, selection) = parse_marked(marked);
        let buffer = Buffer::from_text(&text);
        let ast = parse_document(&text);
        f(Ctx::new(&buffer, &ast, selection))
    }

    #[test]
    fn wrap_selection_in_each_style() {
        for (s, after) in [
            (InlineStyle::Bold, "a **«word»** b"),
            (InlineStyle::Italic, "a *«word»* b"),
            (InlineStyle::Underline, "a ~«word»~ b"),
            (InlineStyle::Strikethrough, "a ~~«word»~~ b"),
            (InlineStyle::Highlight(None), "a ==«word»== b"),
            (
                InlineStyle::Highlight(Some(HighlightColor::Red)),
                "a =={red}«word»== b",
            ),
            (InlineStyle::Code, "a `«word»` b"),
            (InlineStyle::Math, "a $«word»$ b"),
        ] {
            check("a «word» b", after, style(s));
        }
    }

    #[test]
    fn unwrap_selection_in_each_style() {
        for (s, before, after) in [
            (InlineStyle::Bold, "a **«word»** b", "a «word» b"),
            (InlineStyle::Bold, "a «**word**» b", "a «word» b"),
            (InlineStyle::Bold, "a **w«or»d** b", "a w«or»d b"),
            (InlineStyle::Italic, "a *«word»* b", "a «word» b"),
            (InlineStyle::Italic, "a _«word»_ b", "a «word» b"),
            (InlineStyle::Underline, "a ~«word»~ b", "a «word» b"),
            (InlineStyle::Strikethrough, "a ~~«word»~~ b", "a «word» b"),
            (InlineStyle::Highlight(None), "a ==«word»== b", "a «word» b"),
            (InlineStyle::Code, "a `«word»` b", "a «word» b"),
            (InlineStyle::Math, "a $«x»$ b", "a «x» b"),
            (
                InlineStyle::Highlight(Some(HighlightColor::Red)),
                "a =={red}«word»== b",
                "a «word» b",
            ),
        ] {
            check(before, after, style(s));
        }
    }

    #[test]
    fn wrapping_twice_returns_to_the_original() {
        let wrapped = run("a «word» b", style(InlineStyle::Bold)).expect("wrap");
        check(&wrapped, "a «word» b", style(InlineStyle::Bold));
    }

    #[test]
    fn highlight_color_change_and_same_color_toggle() {
        let red = InlineStyle::Highlight(Some(HighlightColor::Red));
        let blue = InlineStyle::Highlight(Some(HighlightColor::Blue));
        check("a =={red}«w»== b", "a =={blue}«w»== b", style(blue));
        check("a ==«w»== b", "a =={red}«w»== b", style(red));
        check(
            "a =={red}«w»== b",
            "a ==«w»== b",
            style(InlineStyle::Highlight(None)),
        );
        check("a =={red}«w»== b", "a «w» b", style(red));
        check("a =={red}x‸== b", "a =={blue}x‸== b", style(blue));
    }

    #[test]
    fn caret_inserts_empty_pair_inside() {
        for (s, after) in [
            (InlineStyle::Bold, "a **‸** b"),
            (InlineStyle::Italic, "a *‸* b"),
            (InlineStyle::Underline, "a ~‸~ b"),
            (InlineStyle::Strikethrough, "a ~~‸~~ b"),
            (InlineStyle::Code, "a `‸` b"),
            (InlineStyle::Math, "a $‸$ b"),
            (InlineStyle::Highlight(None), "a ==‸== b"),
            (
                InlineStyle::Highlight(Some(HighlightColor::Green)),
                "a =={green}‸== b",
            ),
        ] {
            check("a ‸ b", after, style(s));
        }
    }

    #[test]
    fn caret_unwraps_element_it_touches() {
        check("a **wo‸rd** b", "a wo‸rd b", style(InlineStyle::Bold));
        check("a **‸word** b", "a ‸word b", style(InlineStyle::Bold));
        check("a **word‸** b", "a word‸ b", style(InlineStyle::Bold));
        check("a **word**‸ b", "a word‸ b", style(InlineStyle::Bold));
        check("a ‸**word** b", "a ‸word b", style(InlineStyle::Bold));
        check("a `co‸de` b", "a co‸de b", style(InlineStyle::Code));
        // Another style at the caret nests instead of unwrapping.
        check(
            "a **wo‸rd** b",
            "a **wo*‸*rd** b",
            style(InlineStyle::Italic),
        );
    }

    #[test]
    fn nested_same_style_unwraps_the_requested_one() {
        check("***a‸***", "**a‸**", style(InlineStyle::Italic));
        check("***a‸***", "*a‸*", style(InlineStyle::Bold));
    }

    #[test]
    fn selection_trims_whitespace_before_wrapping() {
        check("a« word »b", "a **«word»** b", style(InlineStyle::Bold));
        check_none("a«  »b ", style(InlineStyle::Bold));
    }

    #[test]
    fn wrapping_absorbs_inner_markers_of_the_same_style() {
        check("«**a** b»", "**«a b»**", style(InlineStyle::Bold));
        check("«a **b**»", "**«a b»**", style(InlineStyle::Bold));
        check("«**a** c»", "**«a c»**", style(InlineStyle::Bold));
        // Fully styled pieces separated by whitespace toggle off together.
        check("«**a** **b**»", "«a b»", style(InlineStyle::Bold));
    }

    #[test]
    fn partial_overlaps_snap_to_whole_elements() {
        check("**a «b** c»", "*«**a b** c»*", style(InlineStyle::Italic));
        check("«a **b» c**", "*«a **b c**»*", style(InlineStyle::Italic));
        check("`a«b»c`", "**«`abc`»**", style(InlineStyle::Bold));
        check("[a«b»c](u)", "[a**«b»**c](u)", style(InlineStyle::Bold));
        check("«x [a](u»rl)", "**«x [a](url)»**", style(InlineStyle::Bold));
    }

    #[test]
    fn multi_block_selection_wraps_each_paragraph() {
        check(
            "«one\n\ntwo»",
            "**«one**\n\n**two»**",
            style(InlineStyle::Bold),
        );
        check(
            "«**one**\n\n**two**»",
            "«one\n\ntwo»",
            style(InlineStyle::Bold),
        );
        // Mixed state wraps everything and absorbs the styled part.
        check(
            "«**one**\n\ntwo»",
            "**«one**\n\n**two»**",
            style(InlineStyle::Bold),
        );
    }

    #[test]
    fn table_cells_and_headings_are_regions() {
        check(
            "| a | «b» |\n|---|---|\n| 1 | 2 |",
            "| a | **«b»** |\n|---|---|\n| 1 | 2 |",
            style(InlineStyle::Bold),
        );
        check("# «Title»", "# *«Title»*", style(InlineStyle::Italic));
    }

    #[test]
    fn reversed_selection_stays_reversed() {
        check("a »word« b", "a **»word«** b", style(InlineStyle::Bold));
        check("a **»word«** b", "a »word« b", style(InlineStyle::Bold));
    }

    #[test]
    fn backticks_in_code_content_lengthen_the_fence() {
        check("«a`b»", "``«a`b»``", style(InlineStyle::Code));
        check("«`a»", "`` «`a» ``", style(InlineStyle::Code));
        assert_eq!(longest_backtick_run("a``b`"), 2);
        assert_eq!(longest_backtick_run("ab"), 0);
    }

    #[test]
    fn verbatim_blocks_reject_inline_formatting() {
        check_none("```\nco‸de\n```", style(InlineStyle::Bold));
        check_none("```\n«code»\n```", style(InlineStyle::Bold));
        check_none("$$\nx‸\n$$", style(InlineStyle::Bold));
        // A blank document accepts a pair.
        check("‸", "**‸**", style(InlineStyle::Bold));
    }

    #[test]
    fn caret_inside_atomic_node_inserts_after_it() {
        check("`ab‸c`", "`abc`**‸**", style(InlineStyle::Bold));
    }

    #[test]
    fn turkish_text_is_wrapped_on_char_boundaries() {
        check("«İğdır» ışık", "**«İğdır»** ışık", style(InlineStyle::Bold));
        check("a «ışık»", "a *«ışık»*", style(InlineStyle::Italic));
    }

    #[test]
    fn style_state_at_caret() {
        let state = |before: &str, s| ctx_with(before, |c| is_style_active(c, s));
        assert!(state("**a‸b**", InlineStyle::Bold));
        assert!(state("**ab**‸", InlineStyle::Bold));
        assert!(state("**«ab»**", InlineStyle::Bold));
        assert!(!state("**ab** ‸c", InlineStyle::Bold));
        assert!(!state("**ab**‸", InlineStyle::Italic));
        assert!(state(
            "==a‸==",
            InlineStyle::Highlight(Some(HighlightColor::Blue))
        ));
    }

    #[test]
    fn link_at_reads_inline_links_only() {
        let info = ctx_with("[te‸xt](http://x \"T\")", link_at).expect("link");
        assert_eq!(info.text, "text");
        assert_eq!(info.url, "http://x");
        assert_eq!(info.title.as_deref(), Some("T"));
        assert_eq!(info.range, TextRange::new(0, 20));
        assert!(ctx_with("[te‸xt][r]\n\n[r]: u", link_at).is_none());
        assert!(ctx_with("plain ‸text", link_at).is_none());
        assert!(ctx_with("[text](u)‸ more", link_at).is_some());
        assert!(ctx_with("[a](u)[b](v)‸", link_at).is_some_and(|i| i.text == "b"));
    }

    #[test]
    fn set_link_inserts_and_edits() {
        check("see «this» now", "see [this](https://a.b)‸ now", |c| {
            Some(set_link(c, "this", "https://a.b"))
        });
        check("a ‸ b", "a [x](u)‸ b", |c| Some(set_link(c, "x", "u")));
        check("[o‸ld](http://o)", "[new](http://n)‸", |c| {
            Some(set_link(c, "new", "http://n"))
        });
        check(
            "[old‸](http://o \"T \\\"q\\\"\")",
            "[new](http://n \"T \\\"q\\\"\")‸",
            |c| Some(set_link(c, "new", "http://n")),
        );
        check("‸", "[a\\]b](<a b\\<c\\>>)‸", |c| {
            Some(set_link(c, "a]b", "a b<c>"))
        });
        check("‸", "[[x]](<>)‸", |c| Some(set_link(c, "[x]", "")));
    }

    #[test]
    fn insert_image_replaces_selection() {
        check("a «x» b", "a ![alt](p/i.png)‸ b", |c| {
            Some(insert_image(c, "alt", "p/i.png"))
        });
        check("‸", "![](<my pic.png>)‸", |c| {
            Some(insert_image(c, "", "my pic.png"))
        });
    }

    #[test]
    fn destination_and_bracket_escaping() {
        assert_eq!(link_destination("a(b)"), "<a(b)>");
        assert_eq!(link_destination("a/b.png"), "a/b.png");
        assert_eq!(link_destination(""), "<>");
        assert_eq!(escape_brackets("[a]"), "[a]");
        assert_eq!(escape_brackets("a]["), "a\\]\\[");
        assert_eq!(escape_brackets("[a"), "\\[a");
    }

    #[test]
    fn insert_date_escapes_ordered_list_starts() {
        check("‸", "5\\. Mart 2026‸", |c| {
            Some(insert_date(c, "5. Mart 2026"))
        });
        check("a ‸", "a 5. Mart 2026‸", |c| {
            Some(insert_date(c, "5. Mart 2026"))
        });
        check("- ‸", "- 12\\) Oct‸", |c| {
            Some(insert_date(c, "12) Oct"))
        });
        check("«x»", "Saturday, 10 October 2026‸", |c| {
            Some(insert_date(c, "Saturday, 10 October 2026"))
        });
        // A leading digit without a delimiter is not a list marker.
        check("‸", "2026-10-10‸", |c| {
            Some(insert_date(c, "2026-10-10"))
        });
        // Only ordered markers are escaped.
        check("‸", "- x‸", |c| Some(insert_date(c, "- x")));
    }
}
