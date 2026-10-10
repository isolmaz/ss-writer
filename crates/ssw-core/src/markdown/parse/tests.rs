use crate::markdown::ast::{
    Alignment, Block, BlockKind, CalloutKind, FrontMatterFormat, HighlightColor, ImageKind, Inline,
    InlineKind, LinkKind, ListDelimiter, ReferenceForm,
};
use crate::markdown::definitions::LabelKind;
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

#[test]
fn heading_marker_and_closing_run() {
    let src = "## Title ##\n";
    let b = only_block(src);
    assert!(matches!(b.kind, BlockKind::Heading { level: 2, .. }));
    assert_eq!(texts(src, &b.markers), ["##", "##"]);
    assert_eq!(at(src, b.content), "Title");
    assert_eq!(at(src, b.range), "## Title ##");
    assert_eq!(at(src, b.inlines[0].range), "Title");
}

#[test]
fn setext_underline_is_not_a_heading() {
    let doc = parse_document("Foo\n---\n");
    assert!(matches!(doc.blocks[0].kind, BlockKind::Paragraph));
    assert!(matches!(doc.blocks[1].kind, BlockKind::ThematicBreak));
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
    assert!(matches!(first_inline("~a~").kind, InlineKind::Underline));
    assert!(matches!(
        first_inline("~~a~~").kind,
        InlineKind::Strikethrough
    ));
    assert!(matches!(first_inline("**a**").kind, InlineKind::Strong));
}

#[test]
fn triple_star_nests_strong_inside_emphasis() {
    let src = "***a***";
    let i = first_inline(src);
    assert!(matches!(i.kind, InlineKind::Emphasis));
    assert_eq!(texts(src, &i.markers), ["*", "*"]);
    let inner = &i.children[0];
    assert!(matches!(inner.kind, InlineKind::Strong));
    assert_eq!(texts(src, &inner.markers), ["**", "**"]);
}

#[test]
fn highlight_color_belongs_to_opening_marker() {
    let src = "=={red}x==";
    let i = first_inline(src);
    assert!(matches!(
        i.kind,
        InlineKind::Highlight {
            color: Some(HighlightColor::Red)
        }
    ));
    assert_eq!(texts(src, &i.markers), ["=={red}", "=="]);
    assert_eq!(at(src, i.content), "x");
    assert!(matches!(
        first_inline("==x==").kind,
        InlineKind::Highlight { color: None }
    ));
}

#[test]
fn code_span_markers_and_text() {
    let src = "`` a`b ``";
    let i = first_inline(src);
    assert_eq!(texts(src, &i.markers), ["``", "``"]);
    assert_eq!(at(src, i.content), " a`b ");
    assert!(matches!(&i.kind, InlineKind::Code { text } if text == "a`b"));
}

#[test]
fn inline_link_markers_and_destination() {
    let src = "[text](/url \"t\")";
    let i = first_inline(src);
    assert_eq!(texts(src, &i.markers), ["[", "](/url \"t\")"]);
    assert_eq!(at(src, i.content), "text");
    let InlineKind::Link(link) = &i.kind else {
        panic!("not a link")
    };
    assert_eq!(link.dest, "/url");
    assert_eq!(link.title.as_deref(), Some("t"));
    let LinkKind::Inline {
        dest_range,
        title_range,
    } = &link.kind
    else {
        panic!("not inline")
    };
    assert_eq!(at(src, *dest_range), "/url");
    assert_eq!(at(src, title_range.unwrap()), "t");
}

#[test]
fn reference_link_resolves_through_definitions() {
    let src = "[Foo bar][ref]\n\n[ref]: /url 'title'\n";
    let doc = parse_document(src);
    let link = &doc.blocks[0].inlines[0];
    let InlineKind::Link(l) = &link.kind else {
        panic!("not a link")
    };
    assert_eq!(l.dest, "/url");
    assert_eq!(l.title.as_deref(), Some("title"));
    let LinkKind::Reference {
        label,
        form,
        label_range,
    } = &l.kind
    else {
        panic!("not a reference")
    };
    assert_eq!((label.as_str(), *form), ("ref", ReferenceForm::Full));
    assert_eq!(at(src, *label_range), "ref");
    assert_eq!(texts(src, &link.markers), ["[", "][ref]"]);
    assert!(matches!(
        doc.blocks[1].kind,
        BlockKind::LinkReferenceDefinition(_)
    ));
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
    assert!(matches!(doc.blocks[0].inlines[0].kind, InlineKind::Text));
    assert_eq!(doc.definitions.uses().len(), 1);
}

#[test]
fn image_size_suffix_is_part_of_the_closing_marker() {
    let src = "![alt|300x200](p.png \"T\")";
    let i = first_inline(src);
    let InlineKind::Image(img) = &i.kind else {
        panic!("not an image")
    };
    assert_eq!((img.width, img.height), (Some(300), Some(200)));
    assert_eq!(at(src, img.alt), "alt");
    assert_eq!(texts(src, &i.markers), ["![", "|300x200](p.png \"T\")"]);
    assert_eq!(at(src, i.children[0].range), "alt");
    assert!(matches!(img.kind, ImageKind::Inline { .. }));
    let only_width = first_inline("![a|50](x.png)");
    let InlineKind::Image(img) = &only_width.kind else {
        panic!("not an image")
    };
    assert_eq!((img.width, img.height), (Some(50), None));
}

#[test]
fn html_img_tag_carries_size() {
    let src = "text <img src=\"a.png\" width=\"40\" height=\"30\" alt=\"x\">";
    let doc = parse_document(src);
    let img = &doc.blocks[0].inlines[1];
    let InlineKind::Image(i) = &img.kind else {
        panic!("not an image")
    };
    assert_eq!(
        (i.dest.as_str(), i.width, i.height),
        ("a.png", Some(40), Some(30))
    );
    assert_eq!(at(src, i.alt), "x");
    assert!(matches!(i.kind, ImageKind::Html));
    let block = parse_document("<img src=\"a.png\" width=\"40\">\n");
    assert!(matches!(block.blocks[0].kind, BlockKind::HtmlBlock(_)));
    assert!(matches!(
        block.blocks[0].inlines[0].kind,
        InlineKind::Image(_)
    ));
}

#[test]
fn wikilink_parts() {
    let src = "[[Note#Part|alias]]";
    let i = first_inline(src);
    let InlineKind::WikiLink(w) = &i.kind else {
        panic!("not a wikilink")
    };
    assert_eq!(
        (w.target.as_str(), w.heading.as_deref(), w.alias.as_deref()),
        ("Note", Some("Part"), Some("alias"))
    );
    assert_eq!(texts(src, &i.markers), ["[[", "|", "]]"]);
    assert_eq!(at(src, i.content), "alias");
    assert_eq!(at(src, w.target_range), "Note");
    assert_eq!(at(src, w.heading_range.unwrap()), "Part");
    let plain = first_inline("[[#Head]]");
    let InlineKind::WikiLink(w) = &plain.kind else {
        panic!("not a wikilink")
    };
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
    assert!(matches!(&r.kind, InlineKind::FootnoteRef { label } if label == "1"));
    assert_eq!(texts(src, &r.markers), ["[^", "]"]);
    let BlockKind::FootnoteDefinition { label } = &doc.blocks[1].kind else {
        panic!("no footnote")
    };
    assert_eq!(label, "1");
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
    let InlineKind::Link(l) = &i.kind else {
        panic!("not a link")
    };
    assert_eq!(l.dest, "http://a.b/c");
    assert!(matches!(l.kind, LinkKind::Autolink { angle: true }));
    let mail = first_inline("<me@x.org>");
    assert!(matches!(&mail.kind, InlineKind::Link(l) if l.dest == "mailto:me@x.org"));
    let www = first_inline("www.example.com/a");
    assert!(
        matches!(&www.kind, InlineKind::Link(l) if l.dest == "http://www.example.com/a"
        && matches!(l.kind, LinkKind::Autolink { angle: false }) && www.markers.is_empty())
    );
}

#[test]
fn escapes_entities_and_breaks() {
    let src = "a\\*b &amp; c  \nd\\\ne\nf";
    let doc = parse_document(src);
    let kinds: Vec<&InlineKind> = doc.blocks[0].inlines.iter().map(|i| &i.kind).collect();
    let esc = &doc.blocks[0].inlines[1];
    assert!(matches!(esc.kind, InlineKind::Escape));
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
    assert!(matches!(b.kind, BlockKind::BlockQuote));
    assert_eq!(texts(src, &b.markers), [">", ">"]);
    assert_eq!(at(src, b.children[0].range), "a\n> b");
    assert_eq!(at(src, b.range), "> a\n> b");
}

#[test]
fn lists_markers_tightness_and_tasks() {
    let src = "- [ ] a\n- [x] b\n\n  c\n";
    let list = only_block(src);
    let BlockKind::List {
        ordered: false,
        delimiter: ListDelimiter::Dash,
        tight: false,
        ..
    } = list.kind
    else {
        panic!("unexpected list {:?}", list.kind)
    };
    let first = &list.children[0];
    let BlockKind::ListItem {
        marker,
        task: Some(t),
    } = &first.kind
    else {
        panic!("no task")
    };
    assert_eq!(
        (at(src, *marker), t.checked, at(src, t.checkbox)),
        ("-", false, "[ ]")
    );
    assert_eq!(texts(src, &first.markers), ["-", "[ ]"]);
    assert_eq!(at(src, first.children[0].range), "a");
    let second = &list.children[1];
    assert!(matches!(&second.kind, BlockKind::ListItem { task: Some(t), .. } if t.checked));
    assert_eq!(second.children.len(), 2);

    let tight = only_block("1. a\n2. b\n");
    assert!(matches!(
        tight.kind,
        BlockKind::List {
            ordered: true,
            start: 1,
            delimiter: ListDelimiter::Period,
            tight: true
        }
    ));
    let src = "3) a\n";
    let paren = only_block(src);
    assert!(matches!(
        paren.kind,
        BlockKind::List {
            start: 3,
            delimiter: ListDelimiter::Paren,
            ..
        }
    ));
    assert_eq!(at(src, paren.children[0].markers[0]), "3)");
}

#[test]
fn fenced_and_indented_code() {
    let src = "```rust title\nlet a;\n```\n";
    let b = only_block(src);
    let BlockKind::CodeBlock(code) = &b.kind else {
        panic!("no code")
    };
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
    let BlockKind::CodeBlock(code) = &b.kind else {
        panic!("no code")
    };
    assert!(code.fence.is_none());
    assert_eq!(code.text(src), "a\n\nb\n");

    let open = only_block("~~~\nx");
    let BlockKind::CodeBlock(code) = &open.kind else {
        panic!("no code")
    };
    assert!(code.fence.expect("fenced").close.is_none());
}

#[test]
fn tab_indentation_pads_verbatim_lines() {
    let src = ">\t\tfoo\n";
    let b = only_block(src);
    let BlockKind::CodeBlock(code) = &b.children[0].kind else {
        panic!("no code")
    };
    assert_eq!(code.lines[0].pad, 2);
    assert_eq!(code.text(src), "  foo\n");
}

#[test]
fn table_structure_and_ranges() {
    let src = "| a | b |\n|:--|--:|\n| 1 | **2** |\n";
    let b = only_block(src);
    let BlockKind::Table(t) = &b.kind else {
        panic!("no table")
    };
    assert_eq!(t.alignments, [Alignment::Left, Alignment::Right]);
    assert_eq!(at(src, t.delimiter), "|:--|--:|");
    assert_eq!(texts(src, &b.markers), ["|:--|--:|"]);
    assert_eq!(at(src, t.header.range), "| a | b |");
    assert_eq!(texts(src, &t.header.pipes), ["|", "|", "|"]);
    assert_eq!(at(src, t.header.cells[0].range), " a ");
    assert_eq!(at(src, t.header.cells[0].content), "a");
    assert_eq!(t.rows.len(), 1);
    let strong = &t.rows[0].cells[1].inlines[0];
    assert!(matches!(strong.kind, InlineKind::Strong));
    assert_eq!(texts(src, &strong.markers), ["**", "**"]);
    assert_eq!(at(src, b.range), "| a | b |\n|:--|--:|\n| 1 | **2** |");
    assert!(!matches!(
        parse_document("| a |\n").blocks[0].kind,
        BlockKind::Table(_)
    ));
}

#[test]
fn math_inline_and_display() {
    let src = "a $x^2$ b";
    let i = &parse_document(src).blocks[0].inlines[1];
    assert!(matches!(i.kind, InlineKind::InlineMath));
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
    let BlockKind::MathBlock(m) = &b.kind else {
        panic!("no math")
    };
    assert!(m.closed);
    assert_eq!(m.text(src), "a+b");
    assert_eq!(texts(src, &b.markers), ["$$", "$$"]);
    assert_eq!(at(src, b.range), "$$\na+b\n$$");
    let one = only_block("$$x$$\n");
    assert!(matches!(&one.kind, BlockKind::MathBlock(m) if m.closed && m.text("$$x$$\n") == "x"));
    let unclosed = only_block("$$\nx\n");
    assert!(matches!(&unclosed.kind, BlockKind::MathBlock(m) if !m.closed));
}

#[test]
fn front_matter_yaml_and_toml() {
    let src = "---\ntitle: x\n---\n# H\n";
    let doc = parse_document(src);
    assert!(matches!(
        doc.blocks[0].kind,
        BlockKind::FrontMatter {
            format: FrontMatterFormat::Yaml
        }
    ));
    assert_eq!(texts(src, &doc.blocks[0].markers), ["---", "---"]);
    assert_eq!(at(src, doc.blocks[0].content), "title: x\n");
    assert!(matches!(doc.blocks[1].kind, BlockKind::Heading { .. }));
    let toml = parse_document("+++\na = 1\n+++\n");
    assert!(matches!(
        toml.blocks[0].kind,
        BlockKind::FrontMatter {
            format: FrontMatterFormat::Toml
        }
    ));
    let none = parse_document("---\nfoo\n");
    assert!(matches!(none.blocks[0].kind, BlockKind::ThematicBreak));
}

#[test]
fn callout_tag_title_and_body() {
    let src = "> [!NOTE] My title\n> body text\n";
    let b = only_block(src);
    let BlockKind::Callout { kind, tag, title } = &b.kind else {
        panic!("no callout: {:?}", b.kind)
    };
    assert_eq!(*kind, CalloutKind::Note);
    assert_eq!(at(src, *tag), "[!NOTE]");
    assert_eq!(at(src, title.expect("title")), "My title");
    assert_eq!(texts(src, &b.markers), [">", "[!NOTE]", ">"]);
    assert_eq!(at(src, b.children[0].range), "body text");
    let plain = only_block("> [!NOPE] x\n");
    assert!(matches!(plain.kind, BlockKind::BlockQuote));
}

#[test]
fn page_break_and_thematic_break() {
    let src = "<!-- pagebreak -->\n\n***\n";
    let doc = parse_document(src);
    assert!(matches!(doc.blocks[0].kind, BlockKind::PageBreak));
    assert_eq!(at(src, doc.blocks[0].range), "<!-- pagebreak -->");
    assert!(matches!(doc.blocks[1].kind, BlockKind::ThematicBreak));
    assert_eq!(texts(src, &doc.blocks[1].markers), ["***"]);
}

#[test]
fn raw_html_is_kept_as_nodes() {
    let src = "<div>\nx\n</div>\n\na <b>c</b>\n";
    let doc = parse_document(src);
    let BlockKind::HtmlBlock(h) = &doc.blocks[0].kind else {
        panic!("no html block")
    };
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
