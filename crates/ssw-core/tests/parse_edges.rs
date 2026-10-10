//! Edge cases of the block and inline scanners that the CommonMark examples do not reach:
//! inputs that nearly start a construct (and must stay plain text or paragraphs) and the
//! boundary characters of the delimiter-run, autolink and tag rules. Everything goes through
//! the public `parse_document` entry point.

use ssw_core::markdown::ast::{
    BlockKind, CodeBlock, Inline, InlineKind, Link, LinkKind, MathBlock, iter_inlines,
};
use ssw_core::markdown::parse_document;
use ssw_core::text::TextRange;

// ---------------------------------------------------------------------------------- helpers

/// Kinds of the top-level inlines of the first block.
fn kinds(src: &str) -> Vec<InlineKind> {
    let doc = parse_document(src);
    doc.blocks[0]
        .inlines
        .iter()
        .map(|inline| inline.kind.clone())
        .collect()
}

/// The first block of `src` is a paragraph without a single construct in it.
fn is_plain_paragraph(src: &str) -> bool {
    let doc = parse_document(src);
    doc.blocks.len() == 1
        && doc.blocks[0].kind == BlockKind::Paragraph
        && iter_inlines(&doc.blocks[0].inlines).all(|inline| inline.kind == InlineKind::Text)
}

fn first_inline(src: &str) -> Inline {
    parse_document(src).blocks[0].inlines[0].clone()
}

fn autolink(dest: &str) -> InlineKind {
    InlineKind::Link(Link {
        dest: dest.to_string(),
        title: None,
        kind: LinkKind::Autolink { angle: false },
    })
}

// ------------------------------------------------------------------------------- html blocks

#[test]
fn a_block_tag_name_must_end_at_a_boundary_to_start_an_html_block_of_type_six() {
    let html = |src: &str| {
        let doc = parse_document(src);
        assert_eq!(doc.blocks.len(), 1, "{src:?}");
        let BlockKind::HtmlBlock(block) = &doc.blocks[0].kind else {
            panic!("{src:?} is not an html block: {:?}", doc.blocks[0].kind);
        };
        (block.kind, block.text(src))
    };
    // `/>` closes a block tag, so `<hr/>` is a type 6 block that runs to the blank line.
    assert_eq!(html("<hr/>\nx\n"), (6, "<hr/>\nx\n".to_string()));
    // Any other character behind the name (`-`) makes it a different, custom tag: only a
    // complete tag alone on its line starts a (type 7) block then.
    assert_eq!(html("<div-x>\nx\n"), (7, "<div-x>\nx\n".to_string()));
    assert!(is_plain_paragraph("<hr/x>\n"));
}

#[test]
fn a_complete_tag_of_type_seven_cannot_interrupt_a_paragraph() {
    // A type 6 block interrupts the paragraph, a custom tag of type 7 does not.
    let interrupted = parse_document("para\n<hr/>\n");
    assert_eq!(interrupted.blocks.len(), 2);
    assert_eq!(interrupted.blocks[0].kind, BlockKind::Paragraph);
    assert!(matches!(&interrupted.blocks[1].kind, BlockKind::HtmlBlock(b) if b.kind == 6));
    let continued = parse_document("para\n<div-x>\n");
    assert_eq!(continued.blocks.len(), 1);
    assert_eq!(continued.blocks[0].kind, BlockKind::Paragraph);
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

// ------------------------------------------------------------------------------ other blocks

#[test]
fn a_footnote_label_needs_its_colon_to_open_a_definition() {
    let doc = parse_document("[^a] text\n");
    assert_eq!(doc.blocks.len(), 1);
    assert_eq!(doc.blocks[0].kind, BlockKind::Paragraph);
    assert!(!doc.definitions.has_footnote("a"));
}

#[test]
fn a_footnote_ends_where_its_last_heading_or_fence_info_ends() {
    for (label, source, last) in [
        ("h", "[^h]: a\n\n    # Title\n", "Title"),
        ("c", "[^c]: a\n\n    ```rust\n", "rust"),
    ] {
        let doc = parse_document(source);
        let end = source.find(last).unwrap() + last.len();
        assert_eq!(
            doc.definitions.footnote(label),
            Some(TextRange::new(0, end)),
            "{source:?}"
        );
    }
}

#[test]
fn display_math_on_one_line_must_end_at_its_closing_dollars() {
    // Text behind the closing `$$` keeps the line from being a math block.
    assert!(
        parse_document("$$a$$b\n")
            .blocks
            .iter()
            .all(|block| block.kind == BlockKind::Paragraph)
    );
    // Nothing between the delimiters: a closed block without lines.
    for src in ["$$$$\n", "$$ $$\n"] {
        let doc = parse_document(src);
        assert_eq!(doc.blocks.len(), 1, "{src:?}");
        assert_eq!(
            doc.blocks[0].kind,
            BlockKind::MathBlock(MathBlock {
                lines: Vec::new(),
                closed: true
            }),
            "{src:?}"
        );
    }
}

#[test]
fn a_delimiter_row_without_any_pipe_does_not_turn_a_paragraph_into_a_table() {
    let plain = parse_document("a\n:--\n");
    assert_eq!(plain.blocks.len(), 1);
    assert_eq!(plain.blocks[0].kind, BlockKind::Paragraph);
    // One pipe in the delimiter row is enough.
    let table = parse_document("a\n|-|\n");
    assert_eq!(table.blocks.len(), 1);
    assert!(matches!(table.blocks[0].kind, BlockKind::Table(_)));
}

#[test]
fn text_behind_a_destination_without_a_title_is_not_a_link_definition() {
    let doc = parse_document("[a]: /url trailing\n");
    assert_eq!(doc.blocks.len(), 1);
    assert_eq!(doc.blocks[0].kind, BlockKind::Paragraph);
    assert!(doc.definitions.link("a").is_none());
}

#[test]
fn ordered_list_numbers_up_to_nine_digits_are_read_in_full() {
    let doc = parse_document("999999999. a\n");
    assert!(matches!(
        doc.blocks[0].kind,
        BlockKind::List {
            ordered: true,
            start: 999_999_999,
            ..
        }
    ));
    // Ten digits are too many for a list marker.
    assert_eq!(
        parse_document("1234567890. a\n").blocks[0].kind,
        BlockKind::Paragraph
    );
}

#[test]
fn a_code_block_whose_info_range_is_stale_has_no_language() {
    let stale = CodeBlock {
        fence: None,
        info: Some(TextRange::new(2, 99)),
        lines: Vec::new(),
    };
    assert_eq!(stale.language("```rust\n"), None);
    let covered = CodeBlock {
        info: Some(TextRange::new(3, 7)),
        ..stale
    };
    assert_eq!(covered.language("```rust\n"), Some("rust"));
}

// ----------------------------------------------------------------------------------- inlines

#[test]
fn flanking_looks_at_punctuation_and_whitespace_on_both_sides_of_a_run() {
    // `*` behind a letter and in front of punctuation can only close; the last one is
    // behind punctuation at the end of the text and can only close either.
    assert!(is_plain_paragraph("a*\"foo\"*"));
    // Behind punctuation and in front of a letter it opens.
    assert_eq!(
        kinds("(*foo*)"),
        [InlineKind::Text, InlineKind::Emphasis, InlineKind::Text]
    );
}

#[test]
fn a_colored_highlight_opener_without_content_falls_back_to_a_plain_run() {
    // The color tag alone at the end of the text cannot open a highlight; the `==` is then
    // judged by the `{red}` behind it, with a letter or punctuation in front.
    assert!(is_plain_paragraph("a=={red}"));
    assert!(is_plain_paragraph(".=={red}"));
}

#[test]
fn bracket_text_that_is_not_a_label_cannot_be_a_reference() {
    // The link text holds a bracket pair, so it is no label for `[]` or `[x`.
    assert!(is_plain_paragraph("[a [b] c][]"));
    assert!(is_plain_paragraph("[a [b] c][x"));
}

#[test]
fn a_www_autolink_may_follow_only_some_characters() {
    assert_eq!(
        kinds("(www.example.com)"),
        [
            InlineKind::Text,
            autolink("http://www.example.com"),
            InlineKind::Text
        ]
    );
    assert!(is_plain_paragraph("xwww.example.com"));
}

#[test]
fn an_email_autolink_starts_right_behind_another_inline() {
    assert_eq!(
        kinds("`x`foo@bar.com"),
        [
            InlineKind::Code {
                text: "x".to_string()
            },
            autolink("mailto:foo@bar.com")
        ]
    );
}

#[test]
fn an_angle_autolink_scheme_may_hold_plus_dot_and_dash_but_not_underscore() {
    let scheme = |src: &str| match first_inline(src).kind {
        InlineKind::Link(link) => Some(link.dest),
        _ => None,
    };
    assert_eq!(
        scheme("<git+ssh://host/x>").as_deref(),
        Some("git+ssh://host/x")
    );
    assert_eq!(scheme("<a.b-c:d>").as_deref(), Some("a.b-c:d"));
    assert_eq!(scheme("<a_b:c>"), None);
}

#[test]
fn an_image_size_needs_a_bar_in_front_of_its_digits() {
    for (src, alt) in [
        ("![alt 300](p.png)", "alt 300"),
        ("![300](p.png)", "300"),
        ("![ax300](p.png)", "ax300"),
    ] {
        let inline = first_inline(src);
        let InlineKind::Image(image) = &inline.kind else {
            panic!("{src:?} is not an image");
        };
        assert_eq!((image.width, image.height), (None, None), "{src:?}");
        assert_eq!(image.alt.get(src), Some(alt), "{src:?}");
    }
}

#[test]
fn destinations_and_titles_keep_multi_byte_characters() {
    let inline = first_inline("[a](/日本/😀 \"ü日😀\")");
    let InlineKind::Link(link) = &inline.kind else {
        panic!("not a link");
    };
    assert_eq!(link.dest, "/日本/😀");
    assert_eq!(link.title.as_deref(), Some("ü日😀"));
}
