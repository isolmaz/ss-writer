use super::scan;
use super::source::Source;
use crate::markdown::ast::{
    Alignment, Block, BlockKind, CalloutKind, CodeBlock, FrontMatterFormat, HighlightColor,
    HtmlBlock, Image, ImageKind, Inline, InlineKind, Link, LinkKind, LinkReferenceDefinition,
    ListDelimiter, MathBlock, NodeRef, ReferenceForm, Table, Task, WikiLink,
};
use crate::markdown::definitions::{Definitions, LabelKind, LinkDefinition};
use crate::markdown::parse_document;
use crate::text::TextRange;

fn at(src: &str, r: TextRange) -> &str {
    r.get(src).unwrap_or("<invalid range>")
}

fn texts<'a>(src: &'a str, ranges: &[TextRange]) -> Vec<&'a str> {
    ranges.iter().map(|r| at(src, *r)).collect()
}

fn first_inline(src: &str) -> Inline {
    let doc = parse_document(src);
    doc.blocks[0].inlines[0].clone()
}

fn only_block(src: &str) -> Block {
    let doc = parse_document(src);
    assert_eq!(doc.blocks.len(), 1, "blocks of {src:?}: {:?}", doc.blocks);
    doc.blocks[0].clone()
}

/// Range of the first occurrence of `needle` in `src`.
fn span(src: &str, needle: &str) -> TextRange {
    let start = src.find(needle).unwrap();
    TextRange::new(start, start + needle.len())
}

fn code_of(kind: &BlockKind) -> Option<&CodeBlock> {
    match kind {
        BlockKind::CodeBlock(code) => Some(code),
        _ => None,
    }
}

fn math_of(kind: &BlockKind) -> Option<&MathBlock> {
    match kind {
        BlockKind::MathBlock(math) => Some(math),
        _ => None,
    }
}

fn table_of(kind: &BlockKind) -> Option<&Table> {
    match kind {
        BlockKind::Table(table) => Some(table),
        _ => None,
    }
}

fn html_of(kind: &BlockKind) -> Option<&HtmlBlock> {
    match kind {
        BlockKind::HtmlBlock(html) => Some(html),
        _ => None,
    }
}

fn link_of(kind: &InlineKind) -> Option<&Link> {
    match kind {
        InlineKind::Link(link) => Some(link),
        _ => None,
    }
}

fn image_of(kind: &InlineKind) -> Option<&Image> {
    match kind {
        InlineKind::Image(image) => Some(image),
        _ => None,
    }
}

fn wiki_of(kind: &InlineKind) -> Option<&WikiLink> {
    match kind {
        InlineKind::WikiLink(wiki) => Some(wiki),
        _ => None,
    }
}

#[test]
fn the_payload_helpers_of_these_tests_return_nothing_for_other_kinds() {
    let block = BlockKind::Paragraph;
    assert!(code_of(&block).is_none());
    assert!(math_of(&block).is_none());
    assert!(table_of(&block).is_none());
    assert!(html_of(&block).is_none());
    let inline = InlineKind::Text;
    assert!(link_of(&inline).is_none());
    assert!(image_of(&inline).is_none());
    assert!(wiki_of(&inline).is_none());
}

#[test]
fn heading_marker_and_closing_run() {
    let src = "## Title ##\n";
    let b = only_block(src);
    assert_eq!(b.heading_level(), Some(2));
    assert_eq!(texts(src, &b.markers), ["##", "##"]);
    assert_eq!(at(src, b.content), "Title");
    assert_eq!(at(src, b.range), "## Title ##");
    assert_eq!(at(src, b.inlines[0].range), "Title");
}

#[test]
fn setext_underline_is_not_a_heading() {
    let doc = parse_document("Foo\n---\n");
    assert_eq!(doc.blocks[0].kind, BlockKind::Paragraph);
    assert_eq!(doc.blocks[1].kind, BlockKind::ThematicBreak);
}

#[test]
fn emphasis_family_markers() {
    for (src, open, close) in [
        ("*a*", "*", "*"),
        ("_a_", "_", "_"),
        ("**a**", "**", "**"),
        ("~a~", "~", "~"),
        ("~~a~~", "~~", "~~"),
        ("==a==", "==", "=="),
    ] {
        let i = first_inline(src);
        assert_eq!(texts(src, &i.markers), [open, close], "{src}");
        assert_eq!(at(src, i.content), "a", "{src}");
        assert_eq!(at(src, i.range), src, "{src}");
        assert_eq!(at(src, i.children[0].range), "a");
    }
    assert_eq!(first_inline("~a~").kind, InlineKind::Underline);
    assert_eq!(first_inline("~~a~~").kind, InlineKind::Strikethrough);
    assert_eq!(first_inline("**a**").kind, InlineKind::Strong);
}

#[test]
fn triple_star_nests_strong_inside_emphasis() {
    let src = "***a***";
    let i = first_inline(src);
    assert_eq!(i.kind, InlineKind::Emphasis);
    assert_eq!(texts(src, &i.markers), ["*", "*"]);
    let inner = &i.children[0];
    assert_eq!(inner.kind, InlineKind::Strong);
    assert_eq!(texts(src, &inner.markers), ["**", "**"]);
}

#[test]
fn highlight_color_belongs_to_opening_marker() {
    let src = "=={red}x==";
    let i = first_inline(src);
    assert_eq!(
        i.kind,
        InlineKind::Highlight {
            color: Some(HighlightColor::Red)
        }
    );
    assert_eq!(texts(src, &i.markers), ["=={red}", "=="]);
    assert_eq!(at(src, i.content), "x");
    assert_eq!(
        first_inline("==x==").kind,
        InlineKind::Highlight { color: None }
    );
}

#[test]
fn code_span_markers_and_text() {
    let src = "`` a`b ``";
    let i = first_inline(src);
    assert_eq!(texts(src, &i.markers), ["``", "``"]);
    assert_eq!(at(src, i.content), " a`b ");
    assert_eq!(
        i.kind,
        InlineKind::Code {
            text: "a`b".to_string()
        }
    );
}

#[test]
fn inline_link_markers_and_destination() {
    let src = "[text](/url \"t\")";
    let i = first_inline(src);
    assert_eq!(texts(src, &i.markers), ["[", "](/url \"t\")"]);
    assert_eq!(at(src, i.content), "text");
    let link = link_of(&i.kind).expect("a link");
    assert_eq!(link.dest, "/url");
    assert_eq!(link.title.as_deref(), Some("t"));
    let title = span(src, "\"t\"");
    assert_eq!(
        link.kind,
        LinkKind::Inline {
            dest_range: span(src, "/url"),
            title_range: Some(TextRange::new(title.start + 1, title.end - 1)),
        }
    );
}

#[test]
fn reference_link_resolves_through_definitions() {
    let src = "[Foo bar][ref]\n\n[ref]: /url 'title'\n";
    let doc = parse_document(src);
    let link = &doc.blocks[0].inlines[0];
    let l = link_of(&link.kind).expect("a link");
    assert_eq!(l.dest, "/url");
    assert_eq!(l.title.as_deref(), Some("title"));
    assert_eq!(
        l.kind,
        LinkKind::Reference {
            label: "ref".to_string(),
            form: ReferenceForm::Full,
            label_range: span(src, "ref"),
        }
    );
    assert_eq!(texts(src, &link.markers), ["[", "][ref]"]);
    let label_start = src.rfind("ref").unwrap();
    assert_eq!(
        doc.blocks[1].kind,
        BlockKind::LinkReferenceDefinition(LinkReferenceDefinition {
            label: "ref".to_string(),
            label_range: TextRange::new(label_start, label_start + 3),
            dest: "/url".to_string(),
            dest_range: span(src, "/url"),
            title: Some("title".to_string()),
            title_range: Some(span(src, "title")),
        })
    );
    assert_eq!(at(src, doc.blocks[1].range), "[ref]: /url 'title'");
    assert_eq!(texts(src, &doc.blocks[1].markers), ["[ref]:"]);
    assert!(doc.definitions.link("REF").is_some());
    assert!(
        doc.definitions
            .uses()
            .iter()
            .any(|u| u.kind == LabelKind::Link && u.label == "ref")
    );
}

#[test]
fn undefined_reference_stays_text() {
    let doc = parse_document("[nope]\n");
    assert_eq!(doc.blocks[0].inlines[0].kind, InlineKind::Text);
    assert_eq!(doc.definitions.uses().len(), 1);
}

#[test]
fn image_size_suffix_is_part_of_the_closing_marker() {
    let src = "![alt|300x200](p.png \"T\")";
    let i = first_inline(src);
    let img = image_of(&i.kind).expect("an image");
    assert_eq!((img.width, img.height), (Some(300), Some(200)));
    assert_eq!(at(src, img.alt), "alt");
    assert_eq!(texts(src, &i.markers), ["![", "|300x200](p.png \"T\")"]);
    assert_eq!(at(src, i.children[0].range), "alt");
    assert_eq!(
        img.kind,
        ImageKind::Inline {
            dest_range: span(src, "p.png"),
            title_range: Some(span(src, "T")),
        }
    );
    let only_width = first_inline("![a|50](x.png)");
    let img = image_of(&only_width.kind).expect("an image");
    assert_eq!((img.width, img.height), (Some(50), None));
}

#[test]
fn html_img_tag_carries_size() {
    let src = "text <img src=\"a.png\" width=\"40\" height=\"30\" alt=\"x\">";
    let doc = parse_document(src);
    let img = &doc.blocks[0].inlines[1];
    let i = image_of(&img.kind).expect("an image");
    assert_eq!(
        (i.dest.as_str(), i.width, i.height),
        ("a.png", Some(40), Some(30))
    );
    assert_eq!(at(src, i.alt), "x");
    assert_eq!(i.kind, ImageKind::Html);
    let block = parse_document("<img src=\"a.png\" width=\"40\">\n");
    assert!(html_of(&block.blocks[0].kind).is_some());
    let image = image_of(&block.blocks[0].inlines[0].kind).expect("the lone tag is an image");
    assert_eq!((image.dest.as_str(), image.width), ("a.png", Some(40)));
}

#[test]
fn wikilink_parts() {
    let src = "[[Note#Part|alias]]";
    let i = first_inline(src);
    let w = wiki_of(&i.kind).expect("a wikilink");
    assert_eq!(
        (w.target.as_str(), w.heading.as_deref(), w.alias.as_deref()),
        ("Note", Some("Part"), Some("alias"))
    );
    assert_eq!(texts(src, &i.markers), ["[[", "|", "]]"]);
    assert_eq!(at(src, i.content), "alias");
    assert_eq!(at(src, w.target_range), "Note");
    assert_eq!(at(src, w.heading_range.unwrap()), "Part");
    let plain = first_inline("[[#Head]]");
    let w = wiki_of(&plain.kind).expect("a wikilink");
    assert_eq!(
        (w.target.as_str(), w.heading.as_deref()),
        ("", Some("Head"))
    );
}

#[test]
fn footnote_reference_needs_a_definition() {
    let src = "a[^1]\n\n[^1]: note\n";
    let doc = parse_document(src);
    let r = &doc.blocks[0].inlines[1];
    assert_eq!(
        r.kind,
        InlineKind::FootnoteRef {
            label: "1".to_string()
        }
    );
    assert_eq!(texts(src, &r.markers), ["[^", "]"]);
    assert_eq!(
        doc.blocks[1].kind,
        BlockKind::FootnoteDefinition {
            label: "1".to_string()
        }
    );
    assert_eq!(texts(src, &doc.blocks[1].markers), ["[^1]:"]);
    assert_eq!(at(src, doc.blocks[1].range), "[^1]: note");
    assert!(doc.definitions.has_footnote("1"));
    let missing = parse_document("a[^2]\n");
    assert!(
        missing.blocks[0]
            .inlines
            .iter()
            .all(|i| matches!(i.kind, InlineKind::Text))
    );
}

#[test]
fn angle_and_extended_autolinks() {
    let src = "<http://a.b/c>";
    let i = first_inline(src);
    assert_eq!(texts(src, &i.markers), ["<", ">"]);
    let l = link_of(&i.kind).expect("a link");
    assert_eq!(l.dest, "http://a.b/c");
    assert_eq!(l.kind, LinkKind::Autolink { angle: true });
    let mail = first_inline("<me@x.org>");
    assert_eq!(
        link_of(&mail.kind).map(|l| l.dest.as_str()),
        Some("mailto:me@x.org")
    );
    let www = first_inline("www.example.com/a");
    assert_eq!(
        link_of(&www.kind),
        Some(&Link {
            dest: "http://www.example.com/a".to_string(),
            title: None,
            kind: LinkKind::Autolink { angle: false },
        })
    );
    assert!(www.markers.is_empty());
}

#[test]
fn escapes_entities_and_breaks() {
    let src = "a\\*b &amp; c  \nd\\\ne\nf";
    let doc = parse_document(src);
    let kinds: Vec<&InlineKind> = doc.blocks[0].inlines.iter().map(|i| &i.kind).collect();
    let esc = &doc.blocks[0].inlines[1];
    assert_eq!(esc.kind, InlineKind::Escape);
    assert_eq!(
        (texts(src, &esc.markers), at(src, esc.content)),
        (vec!["\\"], "*")
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, InlineKind::Entity { decoded } if decoded == "&"))
    );
    let hard: Vec<&Inline> = doc.blocks[0]
        .inlines
        .iter()
        .filter(|i| matches!(i.kind, InlineKind::HardBreak))
        .collect();
    assert_eq!(hard.len(), 2);
    assert_eq!(texts(src, &hard[0].markers), ["  "]);
    assert_eq!(at(src, hard[0].range), "  \n");
    assert_eq!(texts(src, &hard[1].markers), ["\\"]);
    assert!(kinds.iter().any(|k| matches!(k, InlineKind::SoftBreak)));
}

#[test]
fn block_quote_has_one_marker_per_line() {
    let src = "> a\n> b\n";
    let b = only_block(src);
    assert_eq!(b.kind, BlockKind::BlockQuote);
    assert_eq!(texts(src, &b.markers), [">", ">"]);
    assert_eq!(at(src, b.children[0].range), "a\n> b");
    assert_eq!(at(src, b.range), "> a\n> b");
}

#[test]
fn lists_markers_tightness_and_tasks() {
    let src = "- [ ] a\n- [x] b\n\n  c\n";
    let list = only_block(src);
    assert_eq!(
        list.kind,
        BlockKind::List {
            ordered: false,
            start: 0,
            delimiter: ListDelimiter::Dash,
            tight: false,
        }
    );
    let first = &list.children[0];
    assert_eq!(
        first.kind,
        BlockKind::ListItem {
            marker: span(src, "-"),
            task: Some(Task {
                checked: false,
                checkbox: span(src, "[ ]"),
            }),
        }
    );
    assert_eq!(texts(src, &first.markers), ["-", "[ ]"]);
    assert_eq!(at(src, first.children[0].range), "a");
    let second = &list.children[1];
    assert_eq!(second.task().map(|t| t.checked), Some(true));
    assert_eq!(second.children.len(), 2);

    let tight = only_block("1. a\n2. b\n");
    assert_eq!(
        tight.kind,
        BlockKind::List {
            ordered: true,
            start: 1,
            delimiter: ListDelimiter::Period,
            tight: true
        }
    );
    let src = "3) a\n";
    let paren = only_block(src);
    assert_eq!(
        paren.kind,
        BlockKind::List {
            ordered: true,
            start: 3,
            delimiter: ListDelimiter::Paren,
            tight: true,
        }
    );
    assert_eq!(at(src, paren.children[0].markers[0]), "3)");
}

#[test]
fn fenced_and_indented_code() {
    let src = "```rust title\nlet a;\n```\n";
    let b = only_block(src);
    let code = code_of(&b.kind).expect("a code block");
    let fence = code.fence.expect("fenced");
    assert_eq!((fence.ch, fence.len), ('`', 3));
    assert_eq!(at(src, fence.open), "```");
    assert_eq!(at(src, fence.close.expect("closed")), "```");
    assert_eq!(at(src, code.info.expect("info")), "rust title");
    assert_eq!(code.language(src), Some("rust"));
    assert_eq!(code.text(src), "let a;\n");
    assert_eq!(at(src, b.content), "let a;");
    assert_eq!(at(src, b.range), "```rust title\nlet a;\n```");
    assert_eq!(texts(src, &b.markers), ["```", "```"]);

    let src = "    a\n\n    b\n";
    let b = only_block(src);
    let code = code_of(&b.kind).expect("a code block");
    assert!(code.fence.is_none());
    assert_eq!(code.text(src), "a\n\nb\n");

    let open = only_block("~~~\nx");
    let code = code_of(&open.kind).expect("a code block");
    assert!(code.fence.expect("fenced").close.is_none());
}

#[test]
fn tab_indentation_pads_verbatim_lines() {
    let src = ">\t\tfoo\n";
    let b = only_block(src);
    let code = code_of(&b.children[0].kind).expect("a code block");
    assert_eq!(code.lines[0].pad, 2);
    assert_eq!(code.text(src), "  foo\n");
}

#[test]
fn table_structure_and_ranges() {
    let src = "| a | b |\n|:--|--:|\n| 1 | **2** |\n";
    let b = only_block(src);
    let t = table_of(&b.kind).expect("a table");
    assert_eq!(t.alignments, [Alignment::Left, Alignment::Right]);
    assert_eq!(at(src, t.delimiter), "|:--|--:|");
    assert_eq!(texts(src, &b.markers), ["|:--|--:|"]);
    assert_eq!(at(src, t.header.range), "| a | b |");
    assert_eq!(texts(src, &t.header.pipes), ["|", "|", "|"]);
    assert_eq!(at(src, t.header.cells[0].range), " a ");
    assert_eq!(at(src, t.header.cells[0].content), "a");
    assert_eq!(t.rows.len(), 1);
    let strong = &t.rows[0].cells[1].inlines[0];
    assert_eq!(strong.kind, InlineKind::Strong);
    assert_eq!(texts(src, &strong.markers), ["**", "**"]);
    assert_eq!(at(src, b.range), "| a | b |\n|:--|--:|\n| 1 | **2** |");
    assert!(table_of(&parse_document("| a |\n").blocks[0].kind).is_none());
}

#[test]
fn math_inline_and_display() {
    let src = "a $x^2$ b";
    let i = &parse_document(src).blocks[0].inlines[1];
    assert_eq!(i.kind, InlineKind::InlineMath);
    assert_eq!(texts(src, &i.markers), ["$", "$"]);
    assert_eq!(at(src, i.content), "x^2");
    let plain = parse_document("costs $5 and $6\n");
    assert!(
        plain.blocks[0]
            .inlines
            .iter()
            .all(|i| matches!(i.kind, InlineKind::Text))
    );

    let src = "$$\na+b\n$$\n";
    let b = only_block(src);
    let m = math_of(&b.kind).expect("a math block");
    assert!(m.closed);
    assert_eq!(m.text(src), "a+b");
    assert_eq!(texts(src, &b.markers), ["$$", "$$"]);
    assert_eq!(at(src, b.range), "$$\na+b\n$$");
    let one = only_block("$$x$$\n");
    let m = math_of(&one.kind).expect("a math block");
    assert!(m.closed);
    assert_eq!(m.text("$$x$$\n"), "x");
    let unclosed = only_block("$$\nx\n");
    assert!(!math_of(&unclosed.kind).expect("a math block").closed);
}

#[test]
fn front_matter_yaml_and_toml() {
    let src = "---\ntitle: x\n---\n# H\n";
    let doc = parse_document(src);
    assert_eq!(
        doc.blocks[0].kind,
        BlockKind::FrontMatter {
            format: FrontMatterFormat::Yaml
        }
    );
    assert_eq!(texts(src, &doc.blocks[0].markers), ["---", "---"]);
    assert_eq!(at(src, doc.blocks[0].content), "title: x\n");
    assert_eq!(doc.blocks[1].heading_level(), Some(1));
    let toml = parse_document("+++\na = 1\n+++\n");
    assert_eq!(
        toml.blocks[0].kind,
        BlockKind::FrontMatter {
            format: FrontMatterFormat::Toml
        }
    );
    let none = parse_document("---\nfoo\n");
    assert_eq!(none.blocks[0].kind, BlockKind::ThematicBreak);
}

#[test]
fn callout_tag_title_and_body() {
    let src = "> [!NOTE] My title\n> body text\n";
    let b = only_block(src);
    assert_eq!(
        b.kind,
        BlockKind::Callout {
            kind: CalloutKind::Note,
            tag: span(src, "[!NOTE]"),
            title: Some(span(src, "My title")),
        }
    );
    assert_eq!(texts(src, &b.markers), [">", "[!NOTE]", ">"]);
    assert_eq!(at(src, b.children[0].range), "body text");
    let plain = only_block("> [!NOPE] x\n");
    assert_eq!(plain.kind, BlockKind::BlockQuote);
}

#[test]
fn callout_range_covers_the_tag_line_and_its_title() {
    for (src, range) in [
        // Title and no body: the range reaches the end of the title.
        ("> [!NOTE] My title", "> [!NOTE] My title"),
        ("> [!NOTE] My title\n\nafter", "> [!NOTE] My title"),
        // No title: the range ends at the tag.
        ("> [!TIP]", "> [!TIP]"),
        // With a body: up to the end of the last child.
        ("> [!NOTE] My title\n> body", "> [!NOTE] My title\n> body"),
        // Nested in a list item.
        ("- a\n\n  > [!WARNING] t", "> [!WARNING] t"),
    ] {
        let doc = parse_document(src);
        let callout = doc
            .iter_blocks()
            .find(|b| matches!(b.kind, BlockKind::Callout { .. }))
            .expect("the source holds a callout");
        assert_eq!(at(src, callout.range), range, "{src:?}");
        assert!(callout.range.contains_range(callout.content));
    }
    let src = "> [!NOTE] My title";
    let b = only_block(src);
    assert_eq!(at(src, b.content), "");
}

#[test]
fn page_break_and_thematic_break() {
    let src = "<!-- pagebreak -->\n\n***\n";
    let doc = parse_document(src);
    assert_eq!(doc.blocks[0].kind, BlockKind::PageBreak);
    assert_eq!(at(src, doc.blocks[0].range), "<!-- pagebreak -->");
    assert_eq!(doc.blocks[1].kind, BlockKind::ThematicBreak);
    assert_eq!(texts(src, &doc.blocks[1].markers), ["***"]);
}

#[test]
fn raw_html_is_kept_as_nodes() {
    let src = "<div>\nx\n</div>\n\na <b>c</b>\n";
    let doc = parse_document(src);
    let h = html_of(&doc.blocks[0].kind).expect("an html block");
    assert_eq!((h.kind, h.text(src)), (6, "<div>\nx\n</div>\n".to_string()));
    assert!(
        doc.blocks[1]
            .inlines
            .iter()
            .any(|i| matches!(i.kind, InlineKind::RawHtml))
    );
}

#[test]
fn path_at_finds_innermost_inline() {
    let src = "a **b** c\n";
    let doc = parse_document(src);
    let node = doc.innermost_at(4).expect("node");
    assert_eq!(at(src, node.range()), "b");
    assert_eq!(doc.len, src.len());
}

#[test]
fn empty_and_blank_documents() {
    assert!(parse_document("").blocks.is_empty());
    assert!(parse_document("\n\n  \n").blocks.is_empty());
}

#[test]
fn list_delimiters_report_their_marker_character() {
    use ListDelimiter::*;
    for (src, delimiter, ch) in [
        ("- a\n", Dash, '-'),
        ("* a\n", Star, '*'),
        ("+ a\n", Plus, '+'),
        ("1. a\n", Period, '.'),
        ("1) a\n", Paren, ')'),
    ] {
        let list = only_block(src);
        let ordered = ".)".contains(ch);
        assert_eq!(
            list.kind,
            BlockKind::List {
                ordered,
                start: u32::from(ordered),
                delimiter,
                tight: true,
            },
            "{src:?}"
        );
        assert_eq!(delimiter.as_char(), ch, "{src:?}");
        assert!(
            at(src, list.children[0].markers[0]).ends_with(ch),
            "{src:?}"
        );
    }
}

#[test]
fn path_at_descends_through_table_rows_cells_and_inlines() {
    let src = "| a | b |\n|---|---|\n| 1 | **2** |\n";
    let doc = parse_document(src);
    let two = src.find('2').expect("digit in the second row");
    let path = doc.path_at(two);

    let names: Vec<&str> = path
        .iter()
        .map(|node| match node {
            NodeRef::Block(_) => "block",
            NodeRef::TableRow(_) => "row",
            NodeRef::TableCell(_) => "cell",
            NodeRef::Inline(_) => "inline",
        })
        .collect();
    assert_eq!(names, ["block", "row", "cell", "inline", "inline"]);
    let ranges: Vec<&str> = path.iter().map(|node| at(src, node.range())).collect();
    assert_eq!(
        ranges,
        [src.trim_end(), "| 1 | **2** |", " **2** ", "**2**", "2"]
    );

    assert_eq!(texts(src, path[0].markers()), ["|---|---|"]);
    assert_eq!(texts(src, path[1].markers()), ["|", "|", "|"]);
    assert!(path[2].markers().is_empty(), "cells have no markers");
    assert_eq!(texts(src, path[3].markers()), ["**", "**"]);
    assert!(path[4].markers().is_empty());

    assert!(path[0].as_block().is_some() && path[0].as_inline().is_none());
    assert!(path[1].as_block().is_none() && path[1].as_inline().is_none());
    assert!(path[3].as_inline().is_some() && path[3].as_block().is_none());

    let innermost = doc.innermost_at(two).expect("inside a cell");
    assert_eq!(at(src, innermost.range()), "2");
    let block = doc.block_at(two).expect("inside the table");
    assert!(table_of(&block.kind).is_some());
}

#[test]
fn path_at_stops_where_no_child_touches_the_offset() {
    let table = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let doc = parse_document(table);
    let delimiter_row = table.find("|---").expect("delimiter row") + 2;
    assert_eq!(doc.path_at(delimiter_row).len(), 1, "a table, but no row");
    assert_eq!(doc.path_at(0).len(), 2, "the leading pipe is in no cell");
    let headerless = parse_document("| a |\n|---|\n");
    assert_eq!(headerless.path_at(8).len(), 1, "a table without body rows");

    let paragraphs = parse_document("a\n\nb\n");
    assert_eq!(paragraphs.path_at(1).len(), 2, "the end of `a` touches it");
    assert!(paragraphs.path_at(2).is_empty(), "between blocks");
    assert!(paragraphs.innermost_at(2).is_none());
    assert!(paragraphs.block_at(2).is_none());
    assert!(
        parse_document("\n\nb\n").path_at(0).is_empty(),
        "before the first block"
    );
    assert!(paragraphs.path_at(1000).is_empty());
}

#[test]
fn inline_groups_headings_and_descendants() {
    let src = "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n*x **y** z*\n";
    let doc = parse_document(src);
    assert_eq!(doc.iter_blocks().count(), 4);
    let levels: Vec<Option<u8>> = doc.blocks.iter().map(Block::heading_level).collect();
    assert_eq!(levels, [Some(1), None, None, None]);

    let group_starts: Vec<Vec<&str>> = doc
        .blocks
        .iter()
        .map(|block| {
            block
                .inline_groups()
                .iter()
                .map(|group| at(src, group[0].range))
                .collect()
        })
        .collect();
    assert_eq!(
        group_starts,
        [
            vec!["Title"],
            vec!["a", "b", "1", "2"],
            Vec::<&str>::new(),
            vec!["*x **y** z*"]
        ]
    );

    let emphasis = &doc.blocks[3].inlines[0];
    let walked: Vec<&str> = emphasis
        .descendants()
        .map(|inline| at(src, inline.range))
        .collect();
    assert_eq!(walked, ["*x **y** z*", "x ", "**y**", "y", " z"]);
}

#[test]
fn definitions_keep_the_first_definition_and_record_each_lookup_once_per_block() {
    let link = |label: &str, dest: &str, title: Option<&str>| LinkDefinition {
        label: label.to_string(),
        dest: dest.to_string(),
        title: title.map(str::to_string),
    };
    let mut defs = Definitions::new();
    assert!(defs.define_link(link("a", "/one", None)));
    assert!(!defs.define_link(link("a", "/two", None)));
    assert!(defs.define_link(link("b", "/b", Some("T"))));
    let mut dests: Vec<&str> = defs.links().map(|l| l.dest.as_str()).collect();
    dests.sort_unstable();
    assert_eq!(dests, ["/b", "/one"]);
    assert_eq!(defs.link("A").map(|l| l.dest.as_str()), Some("/one"));

    let (first, second) = (TextRange::new(10, 20), TextRange::new(30, 40));
    assert!(defs.define_footnote("Note", first));
    assert!(!defs.define_footnote("NOTE", second));
    assert!(defs.define_footnote("other", second));
    let mut labels: Vec<&str> = defs.footnote_labels().collect();
    labels.sort_unstable();
    assert_eq!(labels, ["note", "other"]);
    assert_eq!(defs.footnote("nOTe"), Some(first));
    assert_eq!(defs.footnote("missing"), None);
    assert!(defs.has_footnote("OTHER") && !defs.has_footnote("missing"));

    let (b1, b2) = (TextRange::new(0, 5), TextRange::new(7, 12));
    defs.record_use(b1, LabelKind::Link, "a".into());
    defs.record_use(b1, LabelKind::Link, "a".into());
    defs.record_use(b1, LabelKind::Footnote, "a".into());
    defs.record_use(b2, LabelKind::Link, "a".into());
    defs.record_use(b2, LabelKind::Link, "c".into());
    assert_eq!(
        defs.uses().len(),
        4,
        "a repeated lookup in one block is dropped"
    );
    assert_eq!(
        defs.blocks_using(LabelKind::Link, "A").collect::<Vec<_>>(),
        [b1, b2]
    );
    assert_eq!(
        defs.blocks_using(LabelKind::Footnote, "a")
            .collect::<Vec<_>>(),
        [b1]
    );
    assert_eq!(defs.blocks_using(LabelKind::Link, "zzz").count(), 0);

    let both = [
        (LabelKind::Link, "a".to_string()),
        (LabelKind::Footnote, "a".to_string()),
    ];
    assert_eq!(defs.blocks_affected_by(&both), [b1, b2]);
    assert_eq!(
        defs.blocks_affected_by(&[(LabelKind::Link, "c".to_string())]),
        [b2]
    );
    assert!(defs.blocks_affected_by(&[]).is_empty());
}

#[test]
fn changed_labels_report_additions_removals_and_edits() {
    let link = |label: &str, dest: &str| LinkDefinition {
        label: label.to_string(),
        dest: dest.to_string(),
        title: None,
    };
    let mut left = Definitions::new();
    left.define_link(link("a", "/one"));
    left.define_link(link("b", "/b"));
    left.define_footnote("n", TextRange::new(0, 4));
    left.define_footnote("m", TextRange::new(5, 9));
    let mut right = Definitions::new();
    right.define_link(link("a", "/changed"));
    right.define_link(link("c", "/c"));
    right.define_footnote("n", TextRange::new(20, 30));
    right.define_footnote("o", TextRange::new(31, 35));

    assert_eq!(
        left.changed_labels(&right),
        [
            (LabelKind::Link, "a".to_string()),
            (LabelKind::Link, "b".to_string()),
            (LabelKind::Link, "c".to_string()),
            (LabelKind::Footnote, "m".to_string()),
            (LabelKind::Footnote, "o".to_string()),
        ]
    );
    assert!(left.changed_labels(&left.clone()).is_empty());
}

#[test]
fn html_block_condition_six_accepts_self_closing_block_tags_only() {
    let hr = only_block("<hr/>\n");
    assert_eq!(html_of(&hr.kind).map(|h| h.kind), Some(6));
    let not_a_tag = only_block("<hr/x>\n");
    assert_eq!(not_a_tag.kind, BlockKind::Paragraph);
}

#[test]
fn footnote_range_reaches_the_end_of_a_nested_heading() {
    let src = "[^a]: # Title ##\n";
    let doc = parse_document(src);
    assert_eq!(
        doc.blocks[0].kind,
        BlockKind::FootnoteDefinition {
            label: "a".to_string()
        }
    );
    let range = doc.definitions.footnote("a").expect("defined");
    assert_eq!(at(src, range), "[^a]: # Title ##");
}

#[test]
fn footnote_range_reaches_the_end_of_a_nested_fence_info() {
    // A fence that is never closed has no lines: its info string is the last thing it covers.
    let src = "[^a]: x\n\n    ```rust\n";
    let doc = parse_document(src);
    let range = doc.definitions.footnote("a").expect("defined");
    assert_eq!(at(src, range), "[^a]: x\n\n    ```rust");
}

#[test]
fn malformed_link_reference_definitions_stay_paragraphs() {
    for src in ["[a]: )\n", "[\u{a0}]: /url\n", "[a]:\n", "[a]: <b\n"] {
        let doc = parse_document(src);
        assert_eq!(doc.blocks[0].kind, BlockKind::Paragraph, "{src:?}");
        assert_eq!(doc.definitions.links().count(), 0, "{src:?}");
    }
}

#[test]
fn highlight_color_suffix_that_cannot_open_falls_back_to_a_plain_highlight() {
    let src = "=={red} x==";
    let i = first_inline(src);
    assert_eq!(i.kind, InlineKind::Highlight { color: None });
    assert_eq!(texts(src, &i.markers), ["==", "=="]);
    assert_eq!(at(src, i.content), "{red} x");

    for (src, content) in [
        ("=={bogus}x==", "{bogus}x"),
        ("=={averyverylongcolorname}x==", "{averyverylongcolorname}x"),
        ("=={red x==", "{red x"),
    ] {
        let i = first_inline(src);
        assert_eq!(i.kind, InlineKind::Highlight { color: None }, "{src}");
        assert_eq!(at(src, i.content), content, "{src}");
    }
}

#[test]
fn runs_of_equals_and_tildes_longer_than_a_marker_are_text() {
    // Not at the line start: `~~~` there would open a code fence.
    for src in ["x ===a===", "x ~~~a~~~"] {
        let inlines = parse_document(src).blocks[0].inlines.clone();
        assert_eq!(inlines.len(), 1, "{src}");
        assert_eq!(inlines[0].kind, InlineKind::Text, "{src}");
        assert_eq!(at(src, inlines[0].range), src);
    }
}

#[test]
fn tilde_runs_of_different_lengths_do_not_pair() {
    let src = "~~a~ b~~";
    let i = first_inline(src);
    assert_eq!(i.kind, InlineKind::Strikethrough);
    assert_eq!(at(src, i.content), "a~ b");
}

fn wikilink(src: &str) -> Option<crate::markdown::ast::WikiLink> {
    parse_document(src).blocks[0]
        .inlines
        .iter()
        .find_map(|i| match &i.kind {
            InlineKind::WikiLink(w) => Some(w.clone()),
            _ => None,
        })
}

#[test]
fn wikilink_edge_cases() {
    for src in ["[[]]", "[[a]", "[[a[b]]", "[[a\nb]]", "[[a"] {
        assert_eq!(wikilink(src), None, "{src:?}");
    }
    let w = wikilink("[[|alias]]").expect("alias without a target");
    assert_eq!(
        (w.target.as_str(), w.heading.as_deref(), w.alias.as_deref()),
        ("", None, Some("alias"))
    );
    let w = wikilink("[[a#]]").expect("empty heading");
    assert_eq!(
        (w.target.as_str(), w.heading.as_deref(), w.alias.as_deref()),
        ("a", Some(""), None)
    );
    let w = wikilink("[[a|b|c]]").expect("pipe inside the alias");
    assert_eq!((w.target.as_str(), w.alias.as_deref()), ("a", Some("b|c")));
}

#[test]
fn footnote_syntax_without_a_usable_label_stays_text() {
    for src in ["[^]\n", "[^a b]\n", "[^a\tb]\n", "[^a[b]\n"] {
        let inlines = parse_document(src).blocks[0].inlines.clone();
        assert!(
            inlines.iter().all(|i| matches!(i.kind, InlineKind::Text)),
            "{src:?}"
        );
    }
}

#[test]
fn html_img_tags_without_a_usable_src_stay_raw_html() {
    let doc = parse_document("a <img alt=\"x\"> b <imgx src=\"y\"> c\n");
    let inlines = &doc.blocks[0].inlines;
    let raw = inlines
        .iter()
        .filter(|i| matches!(i.kind, InlineKind::RawHtml))
        .count();
    assert_eq!(raw, 2);
    assert!(
        !inlines
            .iter()
            .any(|i| matches!(i.kind, InlineKind::Image(_)))
    );

    let broken = parse_document("a <img src=> b\n");
    assert!(
        broken.blocks[0]
            .inlines
            .iter()
            .all(|i| matches!(i.kind, InlineKind::Text)),
        "an empty unquoted value is not a tag"
    );
}

#[test]
fn html_img_attributes_are_case_insensitive_and_may_be_unquoted_or_spaced() {
    let src = "x <IMG SRC='a.png' ALT=pic WIDTH=40px HEIGHT = \"tall\" title=\"T\"> y";
    let doc = parse_document(src);
    let img = image_of(&doc.blocks[0].inlines[1].kind).expect("an image");
    assert_eq!(img.dest, "a.png");
    assert_eq!(img.title.as_deref(), Some("T"));
    assert_eq!(at(src, img.alt), "pic");
    assert_eq!((img.width, img.height), (Some(40), None));
    assert_eq!(img.kind, ImageKind::Html);

    let src = "x <img src = \"a.png\"> y";
    let doc = parse_document(src);
    let img = image_of(&doc.blocks[0].inlines[1].kind).expect("an image");
    assert_eq!(img.dest, "a.png");
}

fn math_content(src: &str) -> Option<&str> {
    let doc = parse_document(src);
    doc.blocks[0]
        .inlines
        .iter()
        .find(|i| matches!(i.kind, InlineKind::InlineMath))
        .map(|i| at(src, i.content))
}

#[test]
fn inline_math_delimiter_rules() {
    assert_eq!(math_content("a $$x$$ b"), Some("x"));
    assert_eq!(
        math_content("$a\\$b$"),
        Some("a\\$b"),
        "an escaped dollar is content"
    );
    assert_eq!(
        math_content("x $$a$b$$ y"),
        Some("a$b"),
        "a single dollar inside $$ is content"
    );
    // A `$$` at the line start would be a display math block, not inline math.
    for src in ["a $$$x$$$", "a $ b $ c", "$x$5", "$x $", "a $$b"] {
        assert_eq!(math_content(src), None, "{src:?}");
    }
}

fn links_of(src: &str) -> Vec<(&str, String)> {
    let doc = parse_document(src);
    doc.blocks[0]
        .inlines
        .iter()
        .filter_map(|i| match &i.kind {
            InlineKind::Link(l) => Some((at(src, i.range), l.dest.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn extended_autolinks_follow_the_gfm_boundary_rules() {
    let only = |text: &str, dest: &str| vec![(text.to_string(), dest.to_string())];
    let found = |src: &str| -> Vec<(String, String)> {
        links_of(src)
            .into_iter()
            .map(|(text, dest)| (text.to_string(), dest))
            .collect()
    };
    assert_eq!(
        found("ftp://example.com/file"),
        only("ftp://example.com/file", "ftp://example.com/file")
    );
    assert_eq!(
        found("see (www.a.com) now"),
        only("www.a.com", "http://www.a.com")
    );
    assert_eq!(
        found("http://a.com/b)"),
        only("http://a.com/b", "http://a.com/b")
    );
    assert_eq!(
        found("http://a.com/(b)"),
        only("http://a.com/(b)", "http://a.com/(b)")
    );
    assert_eq!(
        found("x www.a.com/x&amp; y"),
        only("www.a.com/x", "http://www.a.com/x"),
        "a trailing entity reference is not part of the link"
    );
    assert_eq!(
        found("www.a.com/x; y"),
        only("www.a.com/x", "http://www.a.com/x")
    );
    assert_eq!(
        found("www.a.com/&; y"),
        only("www.a.com/&", "http://www.a.com/&")
    );
    assert_eq!(
        found("www.a.com/&a-b; y"),
        only("www.a.com/&a-b", "http://www.a.com/&a-b")
    );
    for src in [
        "http://.",
        "www.a_b.com",
        "www. x",
        "xhttp://a.com",
        "[www.a.com]",
    ] {
        assert!(links_of(src).is_empty(), "{src:?}");
    }
}

#[test]
fn email_autolinks_extend_over_prefixes_and_preceding_tokens() {
    let found = |src: &str| -> Vec<(String, String)> {
        links_of(src)
            .into_iter()
            .map(|(text, dest)| (text.to_string(), dest))
            .collect()
    };
    let one = |text: &str, dest: &str| vec![(text.to_string(), dest.to_string())];
    assert_eq!(found("`x`ab@c.com"), one("ab@c.com", "mailto:ab@c.com"));
    let after_code = parse_document("`x`ab@c.com");
    assert_eq!(
        after_code.blocks[0].inlines[0].kind,
        InlineKind::Code {
            text: "x".to_string()
        }
    );
    assert_eq!(found("x a@b.com"), one("a@b.com", "mailto:a@b.com"));
    assert_eq!(found("a_b@c.com"), one("a_b@c.com", "mailto:a_b@c.com"));
    assert_eq!(
        found("mailto:me@x.org"),
        one("mailto:me@x.org", "mailto:me@x.org")
    );
    assert_eq!(
        found("xmpp:a@b.com/res"),
        one("xmpp:a@b.com/res", "xmpp:a@b.com/res")
    );
    assert_eq!(found("a@b.com."), one("a@b.com", "mailto:a@b.com"));
    for src in ["a@b", "a@b.com-", "@b.com", "a@@b.com"] {
        assert!(links_of(src).is_empty(), "{src:?}");
    }
}

#[test]
fn image_size_suffix_needs_complete_numbers() {
    for (src, alt) in [
        ("![x4](u.png)", "x4"),
        ("![a1x2](u.png)", "a1x2"),
        ("![a|99999999999](u.png)", "a|99999999999"),
        ("![a|99999999999x2](u.png)", "a|99999999999x2"),
    ] {
        let i = first_inline(src);
        let img = image_of(&i.kind).expect("an image");
        assert_eq!((img.width, img.height), (None, None), "{src}");
        assert_eq!(at(src, img.alt), alt, "{src}");
    }
}

#[test]
fn entity_decoding_handles_numeric_named_and_malformed_references() {
    let decode = scan::decode_entity;
    let some = |text: &str, used: usize| Some((text.to_string(), used));
    assert_eq!(decode("x&amp;"), None, "must start at the ampersand");
    assert_eq!(decode("&amp;rest"), some("&", 5));
    assert_eq!(decode("&#65;"), some("A", 5));
    assert_eq!(decode("&#x41;"), some("A", 6));
    assert_eq!(decode("&#X4a;"), some("J", 6));
    assert_eq!(decode("&#0;"), some("\u{fffd}", 4));
    assert_eq!(
        decode("&#xD800;"),
        some("\u{fffd}", 8),
        "surrogates are not scalars"
    );
    for malformed in [
        "&#;",
        "&#12345678;",
        "&#x1234567;",
        "&#65",
        "&bogus;",
        "&;",
        "&amp",
    ] {
        assert_eq!(decode(malformed), None, "{malformed}");
    }
    assert_eq!(
        decode(&format!("&{};", "a".repeat(33))),
        None,
        "name too long"
    );
}

#[test]
fn unescape_resolves_backslashes_and_entities_only() {
    assert_eq!(
        scan::unescape(r"a\*b \a &amp; &copy; &#35; &nope; & é"),
        "a*b \\a & © # &nope; & é"
    );
    assert_eq!(super::unescape_markdown("\\#x &lt;"), "#x <");
    assert_eq!(
        scan::unescape("ü\\*日\\&amp;😀&amp;"),
        "ü*日&amp;😀&",
        "characters of two, three and four bytes pass through whole"
    );
}

#[test]
fn link_labels_need_a_visible_character_and_at_most_999() {
    let end = |s: &str, pos| scan::link_label_end(s.as_bytes(), pos);
    assert_eq!(end("[a]", 1), None, "must start at a bracket");
    assert_eq!(end("[a]", 0), Some(3));
    assert_eq!(end("[ ]", 0), None);
    assert_eq!(end("[a[b]", 0), None);
    assert_eq!(
        end("[a\\]b]", 0),
        Some(6),
        "an escaped bracket stays inside"
    );
    assert_eq!(end("[abc", 0), None, "unterminated");
    let plain = |n: usize| format!("[{}]", "a".repeat(n));
    assert_eq!(end(&plain(999), 0), Some(1001));
    assert_eq!(end(&plain(1000), 0), None);
    let escaped = |n: usize| format!("[{}]", "\\*".repeat(n));
    assert_eq!(end(&escaped(499), 0), Some(1000));
    assert_eq!(end(&escaped(500), 0), None);
}

#[test]
fn link_destinations_cover_angle_and_bare_forms() {
    let dest = |s: &str| scan::link_destination(s.as_bytes(), 0);
    assert_eq!(dest("<a b>"), Some(((1, 4), 5)));
    assert_eq!(dest("<a\\>b>"), Some(((1, 5), 6)));
    assert_eq!(dest("<a"), None);
    assert_eq!(dest("<a\nb>"), None);
    assert_eq!(dest("<a<b>"), None);
    assert_eq!(dest("a(b)c d"), Some(((0, 5), 5)));
    assert_eq!(dest("a(b"), None, "unbalanced parenthesis");
    assert_eq!(dest("a\\)b)"), Some(((0, 4), 4)));
    assert_eq!(
        dest(")"),
        Some(((0, 0), 0)),
        "empty destination before a closing paren"
    );
    assert_eq!(dest(""), None);
}

#[test]
fn link_titles_and_whitespace_skipping() {
    let title = |s: &str| scan::link_title(s.as_bytes(), 0);
    assert_eq!(title("\"a b\""), Some(((1, 4), 5)));
    assert_eq!(title("'a\\'b'"), Some(((1, 5), 6)));
    assert_eq!(title("(a b)"), Some(((1, 4), 5)));
    assert_eq!(
        title("(a(b)"),
        None,
        "unescaped opening paren in a paren title"
    );
    assert_eq!(title("\"abc"), None);
    assert_eq!(title("abc"), None);
    assert_eq!(title(""), None);
    assert_eq!(
        scan::skip_spaces_and_newline(b"  \n  \nx", 0),
        5,
        "one line ending only"
    );
}

#[test]
fn inline_html_forms_end_at_their_closing_delimiter() {
    let end = |s: &str| scan::html_inline_end(s.as_bytes(), 0);
    assert_eq!(scan::html_inline_end(b"a<b>", 0), None, "must start at <");
    assert_eq!(end("<!-- a -->x"), Some(10));
    assert_eq!(end("<!-->x"), Some(5));
    assert_eq!(end("<!--->x"), Some(6));
    assert_eq!(end("<!-- never closed"), None);
    assert_eq!(end("<![CDATA[x]]>y"), Some(13));
    assert_eq!(end("<![CDATA[x"), None);
    assert_eq!(end("<!DOCTYPE html>x"), Some(15));
    assert_eq!(end("<!1>"), None);
    assert_eq!(end("<?php echo ?>x"), Some(13));
    assert_eq!(end("<?x"), None);
    assert_eq!(end("</a >x"), Some(5));
    assert_eq!(end("<a href=x>"), Some(10));
    assert_eq!(end("<1>"), None);
    assert_eq!(end("<"), None);
}

#[test]
fn tags_are_recognized_with_their_attribute_syntax() {
    let closing = |s: &str| scan::closing_tag_end(s.as_bytes(), 0);
    assert_eq!(closing("x</a>"), None);
    assert_eq!(closing("<a>"), None);
    assert_eq!(closing("</1>"), None);
    assert_eq!(closing("</a-b1 \n>"), Some(9));
    assert_eq!(closing("</"), None, "nothing behind the slash");

    let open = |s: &str| scan::open_tag_end(s.as_bytes(), 0);
    assert_eq!(open("<"), None, "nothing behind the bracket");
    assert_eq!(open("<a b="), None, "the value is missing");
    assert_eq!(open("<a/>"), Some(4));
    assert_eq!(open("<a b>"), Some(5));
    assert_eq!(open("<a b= c>"), Some(8));
    assert_eq!(open("<a b=\"c\" d>"), Some(11));
    assert_eq!(open("<a b=>"), None, "empty unquoted value");
    assert_eq!(open("<a b='c>"), None, "unterminated quote");
    assert_eq!(open("<a 1>"), None);
}

#[test]
fn tag_attribute_values_are_located_by_name() {
    fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
        scan::tag_attribute(tag.as_bytes(), name).map(|(start, end)| &tag[start..end])
    }
    let img = "<img src=\"a.png\" alt=x>";
    assert_eq!(attr(img, "src"), Some("a.png"));
    assert_eq!(attr(img, "alt"), Some("x"));
    assert_eq!(attr(img, "width"), None);
    assert_eq!(attr("<img SRC = 'a b' >", "src"), Some("a b"));
    assert_eq!(attr("<img disabled src=a>", "disabled"), Some(""));
    assert_eq!(attr("<img src=", "src"), None);
    assert_eq!(attr("<img src='a", "src"), None);
}

#[test]
fn pixel_lengths_accept_digits_with_an_optional_px_suffix() {
    for (text, want) in [
        ("40", Some(40)),
        ("40px", Some(40)),
        (" 7px ", Some(7)),
        ("", None),
        ("px", None),
        ("4a", None),
        ("-1", None),
        ("99999999999", None),
    ] {
        assert_eq!(scan::parse_pixels(text), want, "{text:?}");
    }
}

#[test]
fn source_maps_joined_lines_back_to_document_offsets() {
    let src = "ab\r\ncd\nef";
    let lines = [
        TextRange::new(0, 2),
        TextRange::new(4, 6),
        TextRange::new(7, 9),
    ];
    let source = Source::from_lines(src, &lines);
    assert_eq!(source.buf, "ab\ncd\nef");
    assert_eq!(source.line_starts, [0, 3, 6]);
    assert_eq!(
        [0, 3, 6, 99].map(|p| source.start(p)),
        [0, 4, 7, 9],
        "positions past the end clamp to the end"
    );
    assert_eq!(
        [0, 2, 3, 6, 8].map(|p| source.end(p)),
        [0, 2, 4, 7, 9],
        "a separator ends after its real terminator (CRLF is two bytes)"
    );
    assert_eq!(source.range(0, 0), TextRange::at(0));
    assert_eq!(source.range(0, 5), TextRange::new(0, 6));
    assert_eq!(source.range(3, 5), TextRange::new(4, 6));

    let single = Source::from_lines("abc", &[TextRange::new(0, 3)]);
    assert_eq!(single.end(3), 3);
    let empty = Source::from_lines("x", &[]);
    assert_eq!(empty.buf, "");
    assert_eq!((empty.start(5), empty.end(0)), (0, 0));
}

#[test]
fn callout_tags_must_be_closed_and_followed_by_a_space() {
    for src in ["> [!NOTE\n", "> [!NOTE]x\n", "> plain\n"] {
        assert_eq!(only_block(src).kind, BlockKind::BlockQuote, "{src:?}");
    }
}

#[test]
fn task_checkboxes_need_a_state_a_space_and_text() {
    for (src, checked) in [
        ("- [ ] a\n", Some(false)),
        ("- [x] a\n", Some(true)),
        ("- [X] a\n", Some(true)),
        ("- [ ]\ta\n", Some(false)),
        ("- [?] a\n", None),
        ("- [x]a\n", None),
        ("- [x]\n", None),
        ("- [ ]  \n", None),
    ] {
        let list = only_block(src);
        assert_eq!(
            list.children[0].task().map(|t| t.checked),
            checked,
            "{src:?}"
        );
    }
}

#[test]
fn highlight_color_and_task_accessors_are_none_for_other_nodes() {
    assert_eq!(
        first_inline("=={green}x==").kind.highlight_color(),
        Some(HighlightColor::Green)
    );
    assert_eq!(first_inline("==x==").kind.highlight_color(), None);
    assert_eq!(first_inline("**x**").kind.highlight_color(), None);

    let list = only_block("- [x] a\n- b\n");
    assert_eq!(list.children[0].task().map(|t| t.checked), Some(true));
    assert_eq!(list.children[1].task(), None, "plain item");
    assert_eq!(list.task(), None, "the list itself");
}

#[test]
fn code_language_and_math_text_of_degenerate_blocks() {
    // An info range that does not address the source (stale tree) has no language.
    let stale = CodeBlock {
        fence: None,
        info: Some(TextRange::new(2, 99)),
        lines: Vec::new(),
    };
    assert_eq!(stale.language("```rust\n"), None);
    let blank_info = CodeBlock {
        info: Some(TextRange::new(0, 2)),
        ..stale
    };
    assert_eq!(blank_info.language("  rust"), None, "only spaces");
    assert_eq!(blank_info.text("  rust"), "", "no lines, no text");

    // Without lines there is no trailing newline to strip.
    let empty = MathBlock {
        lines: Vec::new(),
        closed: false,
    };
    assert_eq!(empty.text("$$\n"), "");
}

// The cases below mirror `tests/parse_edges.rs`: the parser is compiled once for the unit tests
// and once for everything else, and both builds must run these branches.

fn top_kinds(src: &str) -> Vec<InlineKind> {
    let doc = parse_document(src);
    doc.blocks[0]
        .inlines
        .iter()
        .map(|i| i.kind.clone())
        .collect()
}

/// One paragraph in which nothing is a construct.
fn is_plain_paragraph(src: &str) -> bool {
    let doc = parse_document(src);
    doc.blocks.len() == 1
        && doc.blocks[0].kind == BlockKind::Paragraph
        && doc.blocks[0]
            .inlines
            .iter()
            .flat_map(Inline::descendants)
            .all(|i| i.kind == InlineKind::Text)
}

fn autolink(dest: &str) -> InlineKind {
    InlineKind::Link(Link {
        dest: dest.to_string(),
        title: None,
        kind: LinkKind::Autolink { angle: false },
    })
}

#[test]
fn html_block_types_six_and_seven_differ_in_the_tag_boundary_and_in_interrupting() {
    let hr = only_block("<hr/>\nx\n");
    let html = html_of(&hr.kind).expect("an html block");
    assert_eq!(
        (html.kind, html.text("<hr/>\nx\n")),
        (6, "<hr/>\nx\n".into())
    );
    let custom = only_block("<div-x>\nx\n");
    let html = html_of(&custom.kind).expect("an html block");
    assert_eq!(
        (html.kind, html.text("<div-x>\nx\n")),
        (7, "<div-x>\nx\n".into())
    );
    let interrupted = parse_document("para\n<hr/>\n");
    assert_eq!(interrupted.blocks.len(), 2);
    assert_eq!(interrupted.blocks[0].kind, BlockKind::Paragraph);
    assert_eq!(
        html_of(&interrupted.blocks[1].kind).map(|h| h.kind),
        Some(6)
    );
    let continued = only_block("para\n<div-x>\n");
    assert_eq!(continued.kind, BlockKind::Paragraph);
}

#[test]
fn tags_that_the_text_cuts_short_are_not_html() {
    for src in [
        "</\n",
        "<\n",
        "<a b=\n",
        "<a b=>\n",
        "<a b='x\n",
        "x </",
        "x <",
        "x <a b=",
        "x <a b=>",
    ] {
        assert!(is_plain_paragraph(src), "{src:?}");
    }
}

#[test]
fn near_misses_of_blocks_stay_paragraphs() {
    let footnote = parse_document("[^a] text\n");
    assert_eq!(footnote.blocks.len(), 1);
    assert_eq!(footnote.blocks[0].kind, BlockKind::Paragraph);
    assert!(!footnote.definitions.has_footnote("a"));

    let definition = parse_document("[a]: /url trailing\n");
    assert_eq!(definition.blocks.len(), 1);
    assert_eq!(definition.blocks[0].kind, BlockKind::Paragraph);
    assert!(definition.definitions.link("a").is_none());

    let delimiter = only_block("a\n:--\n");
    assert_eq!(delimiter.kind, BlockKind::Paragraph);
    assert!(table_of(&only_block("a\n|-|\n").kind).is_some());

    let after_dollars = parse_document("$$a$$b\n");
    assert!(
        after_dollars
            .blocks
            .iter()
            .all(|b| b.kind == BlockKind::Paragraph)
    );
    for src in ["$$$$\n", "$$ $$\n"] {
        let blank = only_block(src);
        let math = math_of(&blank.kind).expect("a math block");
        assert!(math.closed && math.lines.is_empty(), "{src:?}");
    }

    let nine = only_block("999999999. a\n");
    assert_eq!(
        nine.kind,
        BlockKind::List {
            ordered: true,
            start: 999_999_999,
            delimiter: ListDelimiter::Period,
            tight: true,
        }
    );
    assert_eq!(only_block("1234567890. a\n").kind, BlockKind::Paragraph);
}

#[test]
fn flanking_looks_at_punctuation_and_whitespace_on_both_sides_of_a_run() {
    assert!(is_plain_paragraph("a*\"foo\"*"));
    assert_eq!(
        top_kinds("(*foo*)"),
        [InlineKind::Text, InlineKind::Emphasis, InlineKind::Text]
    );
    // A color tag without content cannot open; the `==` is then judged by what follows it.
    assert!(is_plain_paragraph("a=={red}"));
    assert!(is_plain_paragraph(".=={red}"));
}

#[test]
fn bracket_text_that_is_not_a_label_cannot_be_a_reference() {
    assert!(is_plain_paragraph("[a [b] c][]"));
    assert!(is_plain_paragraph("[a [b] c][x"));
}

#[test]
fn extended_autolinks_depend_on_the_character_in_front() {
    assert_eq!(
        top_kinds("(www.example.com)"),
        [
            InlineKind::Text,
            autolink("http://www.example.com"),
            InlineKind::Text
        ]
    );
    assert!(is_plain_paragraph("xwww.example.com"));
    assert_eq!(
        top_kinds("`x`foo@bar.com"),
        [
            InlineKind::Code {
                text: "x".to_string()
            },
            autolink("mailto:foo@bar.com")
        ]
    );
}

#[test]
fn angle_autolink_schemes_may_hold_plus_dot_and_dash_but_not_underscore() {
    let dest = |src: &str| first_inline(src).kind;
    assert_eq!(
        link_of(&dest("<git+ssh://host/x>")).map(|l| l.dest.as_str()),
        Some("git+ssh://host/x")
    );
    assert_eq!(
        link_of(&dest("<a.b-c:d>")).map(|l| l.dest.as_str()),
        Some("a.b-c:d")
    );
    assert!(link_of(&dest("<a_b:c>")).is_none());
}

#[test]
fn image_sizes_need_a_bar_in_front_of_their_digits() {
    for (src, alt) in [
        ("![alt 300](p.png)", "alt 300"),
        ("![300](p.png)", "300"),
        ("![ax300](p.png)", "ax300"),
    ] {
        let inline = first_inline(src);
        let image = image_of(&inline.kind).expect("an image");
        assert_eq!((image.width, image.height), (None, None), "{src:?}");
        assert_eq!(at(src, image.alt), alt, "{src:?}");
    }
}

#[test]
fn link_destinations_and_titles_keep_multi_byte_characters() {
    let inline = first_inline("[a](/日本/😀 \"ü日😀\")");
    let link = link_of(&inline.kind).expect("a link");
    assert_eq!(link.dest, "/日本/😀");
    assert_eq!(link.title.as_deref(), Some("ü日😀"));
}
