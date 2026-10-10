//! The incremental parser must produce exactly the tree `parse_document` produces for the
//! current text after every edit. Random documents built from Markdown fragments are edited
//! with random insertions, deletions and replacements (fence markers, `$$`, definitions, blank
//! lines, ...); targeted tests cover the cases that change the block structure far away from the
//! edit.

use std::ops::Range;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use ssw_core::edit::Edit;
use ssw_core::markdown::ast::BlockKind;
use ssw_core::markdown::incremental::{IncrementalParser, Reparse};
use ssw_core::markdown::parse_document;
use ssw_core::text::TextRange;

// ---------------------------------------------------------------------------------- helpers

fn assert_equivalent(parser: &IncrementalParser, text: &str) {
    let expected = parse_document(text);
    let actual = parser.document();
    if actual == &expected {
        return;
    }
    let first = actual
        .blocks
        .iter()
        .zip(&expected.blocks)
        .position(|(a, b)| a != b);
    panic!(
        "incremental tree differs from the reference tree: {} vs {} blocks, first differing \
         block {first:?}, definitions equal: {}, length {} vs {}; text: {text:?}",
        actual.blocks.len(),
        expected.blocks.len(),
        actual.definitions == expected.definitions,
        actual.len,
        expected.len,
    );
}

/// Replaces `range` of `text` by `with`, feeds the edit to `parser` and checks equivalence.
fn edit(
    parser: &mut IncrementalParser,
    text: &mut String,
    range: Range<usize>,
    with: &str,
) -> Reparse {
    let edit = Edit::replace(TextRange::new(range.start, range.end), with);
    text.replace_range(range, with);
    let reparse = parser.apply(text, &[edit]);
    assert_equivalent(parser, text);
    reparse
}

/// Offset of the first occurrence of `needle` in `text`.
fn at(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in {text:?}"))
}

fn parser_for(text: &str) -> IncrementalParser {
    let parser = IncrementalParser::new(text);
    assert_equivalent(&parser, text);
    parser
}

// ----------------------------------------------------------------------------- targeted tests

#[test]
fn typing_in_a_paragraph_reparses_that_paragraph_only() {
    let mut text = String::from("# Title\n\nfirst\n\nsecond\n\nthird\n");
    let mut parser = parser_for(&text);
    let at_second = at(&text, "second") + 3;
    let reparse = edit(&mut parser, &mut text, at_second..at_second, "xyz");
    assert_eq!(reparse.blocks_replaced, 2..3);
    assert_eq!(reparse.blocks_inserted, 1);
    assert!(reparse.refreshed.is_empty());
    let block = &parser.document().blocks[2];
    assert_eq!(reparse.changed, block.range);
    assert_eq!(block.range.get(&text), Some("secxyzond"));
}

#[test]
fn deleting_a_whole_document_removes_every_block() {
    let mut text = String::from("# Title\n\nbody\n");
    let mut parser = parser_for(&text);
    let len = text.len();
    let reparse = edit(&mut parser, &mut text, 0..len, "");
    assert_eq!(reparse.blocks_replaced, 0..2);
    assert_eq!(reparse.blocks_inserted, 0);
    assert_eq!(reparse.changed, TextRange::at(0));
    assert!(parser.document().blocks.is_empty());
}

#[test]
fn opening_a_fence_swallows_the_rest_and_closing_it_again_restores_the_tree() {
    let original = "# Title\n\nfirst\n\nsecond\n\nthird\n";
    let mut text = String::from(original);
    let mut parser = parser_for(&text);
    let at_second = at(&text, "second");

    let opened = edit(&mut parser, &mut text, at_second..at_second, "```\n");
    assert_eq!(opened.blocks_replaced, 2..4);
    assert_eq!(opened.blocks_inserted, 1);
    assert_eq!(parser.document().blocks.len(), 3);

    let closed = edit(&mut parser, &mut text, at_second..at_second + 4, "");
    assert_eq!(closed.blocks_replaced, 2..3);
    assert_eq!(closed.blocks_inserted, 2);
    assert_eq!(text, original);
    assert_eq!(parser.document(), &parse_document(original));
}

#[test]
fn a_closing_fence_typed_far_below_splits_the_swallowed_blocks_again() {
    let mut text = String::from("```\nopen\n\nmiddle\n\nlast\n");
    let mut parser = parser_for(&text);
    assert_eq!(parser.document().blocks.len(), 1);
    let end = text.len();
    let closed = edit(&mut parser, &mut text, end..end, "```\n\nafter\n");
    assert_eq!(parser.document().blocks.len(), 2);
    assert_eq!(closed.blocks_replaced, 0..1);
    // The same fence typed again re-opens a block that swallows the end.
    let end = text.len();
    edit(
        &mut parser,
        &mut text,
        end..end,
        "\n```\nswallowed\n\ntail\n",
    );
    assert_eq!(parser.document().blocks.len(), 3);
}

#[test]
fn math_and_html_blocks_that_stay_open_swallow_following_blocks() {
    let mut text = String::from(
        "a\n\n$$\nx\n$$\n\nb\n\n<!-- note -->\n\nc\n\n<script>\nlet x;\n</script>\n\nd\n",
    );
    let mut parser = parser_for(&text);
    let math = at(&text, "$$\nx\n$$") + 3;
    edit(&mut parser, &mut text, math..math, "$$\n");
    let comment = at(&text, "-->");
    edit(&mut parser, &mut text, comment..comment + 3, "");
    let script = at(&text, "</script>");
    edit(&mut parser, &mut text, script..script + 9, "");
    let comment = at(&text, "<!--");
    edit(&mut parser, &mut text, comment..comment, "--> ");
}

#[test]
fn a_new_link_definition_changes_earlier_paragraphs_and_removing_it_changes_them_back() {
    let mut text = String::from("See [foo] and [^n].\n\nmid\n\nSee [foo][] too.\n");
    let mut parser = parser_for(&text);
    let end = text.len();
    let added = edit(
        &mut parser,
        &mut text,
        end..end,
        "\n[foo]: /url \"T\"\n\n[^n]: A note.\n",
    );
    assert!(added.refreshed.contains(&0), "{added:?}");
    assert!(!added.refreshed.contains(&1), "{added:?}");
    assert_eq!(
        parser
            .document()
            .definitions
            .link("foo")
            .map(|d| d.dest.as_str()),
        Some("/url")
    );
    assert!(parser.document().definitions.has_footnote("n"));

    let def = at(&text, "\n[foo]");
    let len = text.len();
    let removed = edit(&mut parser, &mut text, def..len, "");
    assert!(removed.refreshed.contains(&0), "{removed:?}");
    assert!(parser.document().definitions.link("foo").is_none());
}

#[test]
fn changing_a_definition_value_refreshes_every_reader_but_a_shadowed_duplicate_changes_nothing() {
    let mut text = String::from(
        "[a]: /one\n\nuse [a]\n\n[A]: /two\n\nuse [a] again\n\nand [b]\n\n[b]: /b 'B'\n",
    );
    let mut parser = parser_for(&text);
    // Change the winning definition's destination.
    let one = at(&text, "/one");
    let changed = edit(&mut parser, &mut text, one..one + 4, "/uno");
    assert!(changed.refreshed.contains(&1), "{changed:?}");
    assert!(changed.refreshed.contains(&3), "{changed:?}");
    // Editing the shadowed duplicate leaves the readers alone.
    let two = at(&text, "/two");
    let shadow = edit(&mut parser, &mut text, two..two + 4, "/dos");
    assert!(shadow.refreshed.is_empty(), "{shadow:?}");
    // Deleting the winner promotes the duplicate for every reader.
    let first = at(&text, "[a]: /uno\n\n");
    let promoted = edit(&mut parser, &mut text, first..first + 11, "");
    assert!(!promoted.refreshed.is_empty());
    assert_eq!(
        parser
            .document()
            .definitions
            .link("a")
            .map(|d| d.dest.as_str()),
        Some("/dos")
    );
}

#[test]
fn references_inside_a_window_resolve_against_definitions_before_and_behind_it() {
    let mut text = String::from(
        "[early]: /early\n\n[^e]: early note\n\nbody [early] [late] [^e] [^l]\n\n[late]: /late\n\n[^l]: late note\n\ntail\n",
    );
    let mut parser = parser_for(&text);
    let body = at(&text, "body") + 4;
    for piece in ["x", " [early][late]", "\n[^e]"] {
        let reparse = edit(&mut parser, &mut text, body..body, piece);
        assert!(reparse.refreshed.is_empty(), "{reparse:?}");
    }
    // A label defined both before and inside the window: the earlier definition wins.
    let body = at(&text, "body");
    edit(
        &mut parser,
        &mut text,
        body..body,
        "[early]: /shadow\n[x]: /x\n\n",
    );
    let early = at(&text, "[early]: /early\n\n");
    edit(&mut parser, &mut text, early..early + 17, "");
}

#[test]
fn definitions_with_awkward_destinations_and_titles_survive_being_supplied_to_a_window() {
    let defs = "[odd]: <a\\<b\\>c&amp;d&#10;e&#13;f\\\\g> \"t\\\"q&amp;&#10;x\\\\\"\n\n\
                [^ Ünï]: foot\n\n\
                [ ^caret]: /caret\n\n\
                [empty]: <>\n\n\
                [ΣΑΣ]: /sigma 'single'\n\n\
                [paren]: /p (a title)\n\n";
    let mut text = format!("{defs}use [odd] [ΣΑΣ] [empty] [^caret] [paren] [^Ünï] [ ^caret]\n");
    let mut parser = parser_for(&text);
    let usage = at(&text, "use") + 3;
    edit(&mut parser, &mut text, usage..usage, "d");
    edit(&mut parser, &mut text, usage..usage + 1, "");
    let definitions = &parser.document().definitions;
    let odd = definitions
        .link("odd")
        .map(|d| (d.dest.as_str(), d.title.as_deref()));
    assert_eq!(odd, Some(("a<b>c&d\ne\rf\\g", Some("t\"q&\nx\\"))));
    assert_eq!(
        definitions.link("^caret").map(|d| d.dest.as_str()),
        Some("/caret")
    );
}

#[test]
fn front_matter_delimiters_are_found_and_lost_anywhere_in_the_document() {
    let original = "---\ntitle: x\n---\n\nbody\n\n---\n\nmore\n";
    let mut text = String::from(original);
    let mut parser = parser_for(&text);
    // Deleting the closing delimiter makes the front matter end at the next `---`.
    let close = at(&text, "---\n\nbody");
    let lost = edit(&mut parser, &mut text, close..close + 4, "");
    assert_eq!(lost.blocks_replaced.start, 0);
    // Restoring it brings the original tree back.
    let restored = edit(&mut parser, &mut text, close..close, "---\n");
    assert_eq!(restored.blocks_replaced.start, 0);
    assert_eq!(text, original);
    // Deleting the opening delimiter removes the front matter altogether.
    edit(&mut parser, &mut text, 0..4, "");
    edit(&mut parser, &mut text, 0..0, "---\n");
    // Editing inside the front matter body.
    let key = at(&text, "title");
    edit(&mut parser, &mut text, key..key, "sub");
    // Typing a closing delimiter far below an unclosed opening one creates front matter.
    let mut open = String::from("---\nkey: v\n\ntext\n\nmore text\n");
    let mut parser = parser_for(&open);
    let end = open.len();
    let created = edit(&mut parser, &mut open, end..end, "---\n\nafter\n");
    assert_eq!(created.blocks_replaced.start, 0);
    // The other front matter flavours.
    for (start, close) in [("+++\n", "+++\n"), ("---\n", "...\n")] {
        let mut text = format!("{start}k = 1\n{close}\nbody\n");
        let mut parser = parser_for(&text);
        let at_close = text
            .rfind(close)
            .unwrap_or_else(|| panic!("{close:?} not found in {text:?}"));
        let tail = text.len();
        edit(&mut parser, &mut text, at_close..at_close + 1, "x");
        edit(&mut parser, &mut text, tail..tail, "tail\n");
    }
    // CRLF and trailing blanks on the delimiter lines.
    let mut text = String::from("---  \r\ntitle: x\r\n---\t\r\n\r\nbody\r\n");
    let mut parser = parser_for(&text);
    let body = at(&text, "body");
    edit(&mut parser, &mut text, body..body, "more ");
    let title = at(&text, "title");
    edit(&mut parser, &mut text, title..title, "a");
}

#[test]
fn front_matter_without_a_closing_line_is_not_front_matter() {
    for source in ["---", "---\n", "---\nkey: v", "+++\nk = 1\n", "--- \n"] {
        let mut text = String::from(source);
        let mut parser = parser_for(&text);
        let end = text.len();
        edit(&mut parser, &mut text, end..end, "\nmore");
        let end = text.len();
        edit(&mut parser, &mut text, end..end, "\n---");
    }
}

#[test]
fn a_front_matter_fence_that_is_the_whole_text_opens_nothing_until_it_is_continued() {
    let front_matter = |parser: &IncrementalParser| {
        matches!(
            parser.document().blocks.first().map(|block| &block.kind),
            Some(BlockKind::FrontMatter { .. })
        )
    };
    for (source, fence) in [("---\nkey: v", "---"), ("+++\nk = 1", "+++")] {
        let mut text = String::from(source);
        let mut parser = parser_for(&text);
        let len = text.len();
        // The fence line is the entire text: there is no line break after it.
        edit(&mut parser, &mut text, 3..len, "");
        assert_eq!(text, fence);
        assert!(!front_matter(&parser), "{text:?}");
        let end = text.len();
        edit(&mut parser, &mut text, end..end, &format!("\nkey\n{fence}"));
        assert!(front_matter(&parser), "{text:?}");
    }
}

#[test]
fn lazy_continuation_merges_and_unmerges_blocks() {
    for source in [
        "- item\n\nlazy text\n",
        "> quote\n\nlazy text\n",
        "1. item\n\n   continued\n\nnext\n",
        "para\n\n    code\n\n    more\n",
        "[^a]: note\n\n    more\n\nnext\n",
    ] {
        let mut text = String::from(source);
        let mut parser = parser_for(&text);
        // Remove the first blank line, then put it back.
        let blank = at(&text, "\n\n") + 1;
        edit(&mut parser, &mut text, blank..blank + 1, "");
        edit(&mut parser, &mut text, blank..blank, "\n");
        // Add text right behind the first block without a blank line.
        edit(&mut parser, &mut text, blank..blank, "more\n");
    }
}

#[test]
fn lists_footnotes_and_indented_code_pull_in_the_following_blocks() {
    let mut text = String::from(
        "intro\n\n- one\n- two\n\nbetween\n\n[^a]: note\n\nafter note\n\n    code\n\nafter code\n\nend\n",
    );
    let mut parser = parser_for(&text);
    for needle in ["two", "note", "code", "between", "end"] {
        let at_needle = at(&text, needle);
        edit(&mut parser, &mut text, at_needle..at_needle, "x");
    }
    // Turn the paragraph after the list into a list item of the same list.
    let between = at(&text, "xbetween");
    edit(&mut parser, &mut text, between..between, "- ");
    // And back into text.
    edit(&mut parser, &mut text, between..between + 2, "");
}

#[test]
fn multibyte_text_shifts_cleanly() {
    let mut text =
        String::from("Türkçe ığş İstanbul\n\n😀 *emoji* ünïcödé\n\n[x]: /ü\n\nsonra [x] 🎉\n");
    let mut parser = parser_for(&text);
    let first = at(&text, "ığş");
    edit(&mut parser, &mut text, first..first, "çöğüşı😀");
    let emoji = at(&text, "😀 *");
    edit(&mut parser, &mut text, emoji..emoji + "😀".len(), "");
    let sonra = at(&text, "sonra");
    edit(&mut parser, &mut text, sonra..sonra, "İ");
    let end = text.len();
    edit(&mut parser, &mut text, end..end, "\n\nğ\n");
    let first = at(&text, "Türkçe");
    edit(&mut parser, &mut text, first..first + "Türkçe".len(), "");
}

#[test]
fn single_character_edits_in_a_ten_thousand_block_document_reparse_at_most_three_blocks() {
    let mut text = String::new();
    for i in 0..10_000 {
        text.push_str(&format!(
            "Paragraph number {i} with *emphasis* and [ref].\n\n"
        ));
    }
    let mut parser = parser_for(&text);
    assert_eq!(parser.document().blocks.len(), 10_000);
    let middle = at(&text, "Paragraph number 5000 ") + 4;
    let typed = edit(&mut parser, &mut text, middle..middle + 1, "X");
    assert!(typed.blocks_replaced.len() <= 3, "{typed:?}");
    assert!(typed.blocks_inserted <= 3, "{typed:?}");
    let inserted = edit(&mut parser, &mut text, middle..middle, "y");
    assert!(inserted.blocks_replaced.len() <= 3, "{inserted:?}");
    let deleted = edit(&mut parser, &mut text, middle..middle + 2, "");
    assert!(deleted.blocks_replaced.len() <= 3, "{deleted:?}");
    assert!(deleted.blocks_inserted <= 3, "{deleted:?}");
    assert!(deleted.refreshed.is_empty());
}

#[test]
fn shifting_moves_every_kind_of_node() {
    let rest = concat!(
        "# Heading with `code` and *emph* #\n\n",
        "Paragraph **strong** ~under~ ~~strike~~ ==high== =={red}colored== $x^2$ \\* &amp; <span>raw</span> hard\\\nbreak soft\nbreak <https://example.com> www.example.org ",
        "[link](http://a.b \"ti\") [ref][r] [r][] [r] ![img](a.png \"t\") ![alt|30x20][r] <img src=\"x.png\" width=\"10\"> ",
        "[[Page#Head|alias]] [[#only]] [[plain]] [^1] [^missing] [missing]\n\n",
        "- [ ] task item\n- [x] done\n  - nested\n\n1. one\n2. two\n\n",
        "> [!NOTE] Title\n> body\n\n> [!TIP]\n\n> plain quote\n> > inner\n\n",
        "```rust title\ncode\n```\n\n~~~\ntilde\n~~~\n\n    indented\n\n",
        "$$\nx\n$$\n\n$$y$$\n\n",
        "| a | b |\n|:-|-:|\n| *1* | [r] |\n| 2 |\n\n",
        "<div>\nhtml\n</div>\n\n<img src=\"a.png\" alt=\"x\" width=\"5\" height=\"6\">\n\n<!-- pagebreak -->\n\n***\n\n",
        "[^1]: Footnote text.\n\n    second para\n\n",
        "[r]: http://r.example \"Title\"\n",
    );
    for front in ["", "---\ntitle: x\n---\n\n", "+++\nk = 1\n+++\n\n"] {
        let mut text = format!("{front}first paragraph\n\n{rest}");
        let mut parser = parser_for(&text);
        let first = at(&text, "first");
        // Grow, then shrink: every later node moves right, then left.
        edit(
            &mut parser,
            &mut text,
            first..first,
            "a longer start of the ",
        );
        let first = at(&text, "a longer start of the ");
        edit(&mut parser, &mut text, first..first + 22, "");
        // Edit the paragraph full of references: it is parsed with outside definitions.
        let paragraph = at(&text, "Paragraph **strong**");
        edit(&mut parser, &mut text, paragraph..paragraph, "x ");
        let heading = at(&text, "Heading");
        edit(&mut parser, &mut text, heading..heading, "More ");
    }
}

#[test]
fn crlf_documents_stay_equivalent() {
    let mut text = String::from(
        "# T\r\n\r\npara one\r\n\r\n- a\r\n- b\r\n\r\n```\r\ncode\r\n```\r\n\r\n[r]: /u\r\n\r\nsee [r]\r\n",
    );
    let mut parser = parser_for(&text);
    let para = at(&text, "para");
    edit(&mut parser, &mut text, para..para, "x");
    let blank = at(&text, "\r\n\r\n- a") + 2;
    edit(&mut parser, &mut text, blank..blank + 2, "");
    edit(&mut parser, &mut text, blank..blank, "\r\n");
    let code = at(&text, "code");
    edit(&mut parser, &mut text, code..code, "```\r\n");
}

#[test]
fn transactions_with_several_edits_are_applied_as_one() {
    let mut text = String::from("alpha\n\nbeta\n\ngamma\n\ndelta\n");
    let mut parser = parser_for(&text);
    let beta = at(&text, "beta");
    let delta = at(&text, "delta");
    let edits = [
        Edit::replace(TextRange::new(beta, beta + 4), "BETA!!"),
        Edit::insert(delta, "[x]: /u\n\n"),
    ];
    text.replace_range(delta..delta, "[x]: /u\n\n");
    text.replace_range(beta..beta + 4, "BETA!!");
    parser.apply(&text, &edits);
    assert_equivalent(&parser, &text);
}

#[test]
fn edits_that_do_not_match_the_texts_make_the_parser_start_over() {
    let mut text = String::from("one\n\ntwo\n");
    let mut parser = parser_for(&text);

    // No edits but a different text.
    text.push_str("\nthree\n");
    let reparse = parser.apply(&text, &[]);
    assert_eq!(reparse.blocks_replaced, 0..2);
    assert_eq!(reparse.blocks_inserted, 3);
    assert_eq!(reparse.changed, TextRange::new(0, text.len()));
    assert_equivalent(&parser, &text);

    // An edit past the end of the previous text.
    text.push_str("four\n");
    let reparse = parser.apply(&text, &[Edit::insert(10_000, "x")]);
    assert_eq!(reparse.blocks_replaced, 0..3);
    assert_equivalent(&parser, &text);

    // Edits whose length change contradicts the new text: the text got shorter although the
    // edit only inserts.
    text.remove(0);
    let reparse = parser.apply(&text, &[Edit::insert(5, "x")]);
    assert_eq!(reparse.blocks_replaced, 0..3);
    assert_equivalent(&parser, &text);

    // Nothing changed.
    let reparse = parser.apply(&text, &[]);
    assert_eq!(reparse.blocks_replaced, 0..0);
    assert_eq!(reparse.blocks_inserted, 0);
    assert!(reparse.refreshed.is_empty());
    assert_equivalent(&parser, &text);
}

#[test]
fn tables_html_and_callouts_resync_after_edits_next_to_them() {
    let mut text = String::from(
        "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> [!NOTE] Hi\n> body\n\n<div>\nx\n</div>\n\ntail\n",
    );
    let mut parser = parser_for(&text);
    // Drop the delimiter row's dashes, then restore them.
    let dashes = at(&text, "|---|---|");
    edit(&mut parser, &mut text, dashes..dashes + 9, "|-|");
    edit(&mut parser, &mut text, dashes..dashes + 3, "|---|---|");
    // Break the callout tag and repair it.
    let tag = at(&text, "[!NOTE]");
    edit(&mut parser, &mut text, tag + 1..tag + 2, "");
    edit(&mut parser, &mut text, tag + 1..tag + 1, "!");
    // Remove the html block's closing tag so it swallows less than before, then restore.
    let close = at(&text, "</div>");
    edit(&mut parser, &mut text, close..close + 6, "");
    edit(&mut parser, &mut text, close..close, "</div>");
    // Typing at the very start and very end.
    edit(&mut parser, &mut text, 0..0, "# ");
    let end = text.len();
    edit(&mut parser, &mut text, end..end, "x");
}

#[test]
fn an_outside_footnote_cannot_take_over_the_first_lines_of_a_window() {
    // The window parse is handed the footnote definition its references need. A footnote stays
    // open across blank lines and continues with lines indented by four spaces, so a window that
    // starts with indented code would be swallowed by it if the definition were supplied with
    // content.
    let mut text = String::from("[^1]: note\n\nintro\n\n    code\nsee[^1]\n\ntail\n");
    let mut parser = parser_for(&text);
    let code = at(&text, "code");
    let reparse = edit(&mut parser, &mut text, code..code, "x");
    assert_eq!(reparse.blocks_replaced, 2..4);
    assert_eq!(reparse.blocks_inserted, 2);
    assert!(reparse.refreshed.is_empty(), "{reparse:?}");
    let blocks = &parser.document().blocks;
    assert_eq!(blocks.len(), 5);
    assert!(matches!(blocks[2].kind, BlockKind::CodeBlock(_)));
    assert!(matches!(blocks[3].kind, BlockKind::Paragraph));
    assert_eq!(blocks[2].range.get(&text), Some("xcode"));
}

#[test]
fn replaying_the_footnote_regression_found_by_the_property_test() {
    // Minimal input the property test shrank to: the last edit types a heading into the
    // indented code block that follows a footnote definition, in a document that references that
    // footnote further down.
    let mut text = String::from(
        "[^two]: Second\n\n    indented continuation\nNote ref[^1] and[^two].\n\n\
         <!-- pagebreak -->\nSee [foo], [bar][foo] and [baz].\n\n\nTürkçe ığş çö 😀 text\n\n\
         * [ ] task\n* [x] done\n\n\n> [!WARNING]\n\n---\ntitle: Doc\n---\n\n[Foo]\n\n",
    );
    let mut parser = parser_for(&text);
    let edits: [(Range<usize>, &str); 5] = [
        (217..218, ""),
        (64..64, "```"),
        (7..7, "<!-- comment -->\n"),
        (234..236, ""),
        (37..37, "# Heading\n"),
    ];
    for (range, with) in edits {
        edit(&mut parser, &mut text, range, with);
    }
    assert_eq!(parser.document().blocks.len(), 13);
    assert!(matches!(
        parser.document().blocks[2].kind,
        BlockKind::CodeBlock(_)
    ));
}

#[test]
fn footnote_ranges_follow_edits_for_every_kind_of_last_child() {
    // The range of a footnote definition ends where its last child ends; the incremental parser
    // derives it from the tree, so every container kind has to report its raw end.
    let mut text = String::from(concat!(
        "intro\n\n",
        "[^list]: a\n\n    - item\n    - [x] task\n\n",
        "[^quote]: a\n\n    > quoted\n\n",
        "[^titled]: a\n\n    > [!NOTE] Title\n\n",
        "[^bare]: a\n\n    > [!TIP]\n\n",
        "[^body]: a\n\n    > [!TIP] Tip\n    > and a body\n\n",
        "[^deep]: a\n\n    [^inner]: nested\n\n",
        "tail\n",
    ));
    let mut parser = parser_for(&text);
    // Everything behind the paragraph moves, then moves back.
    let intro = at(&text, "intro");
    edit(&mut parser, &mut text, intro..intro, "more ");
    edit(&mut parser, &mut text, intro..intro + 5, "");
    // Edits inside the footnotes parse them again.
    for needle in ["item", "quoted", "Title", "[!TIP]\n\n", "nested"] {
        let at_needle = at(&text, needle);
        edit(&mut parser, &mut text, at_needle..at_needle, "x");
    }
    let definitions = &parser.document().definitions;
    for label in ["list", "quote", "titled", "bare", "body", "deep", "inner"] {
        assert!(definitions.has_footnote(label), "{label}");
    }
}

#[test]
fn every_leaf_block_of_a_list_that_reads_a_changed_label_is_parsed_again_once() {
    let mut text = String::from("- see [foo]\n- also [foo]\n\nmid\n\nend\n");
    let mut parser = parser_for(&text);
    let end = text.len();
    let added = edit(&mut parser, &mut text, end..end, "\n[foo]: /u\n");
    assert_eq!(added.blocks_replaced, 2..3);
    assert_eq!(added.blocks_inserted, 2);
    // Both paragraphs sit in the same top-level list.
    assert_eq!(added.refreshed, vec![0]);
}

#[test]
fn a_definition_in_the_window_beats_a_later_one_for_references_in_the_window() {
    let mut text =
        String::from("[late]: /one\nuse [late] and [^n]\n\n[late]: /two\n\n[^n]: note\n");
    let mut parser = parser_for(&text);
    let usage = at(&text, "use");
    let reparse = edit(&mut parser, &mut text, usage..usage, "x");
    assert!(reparse.refreshed.is_empty(), "{reparse:?}");
    let dest = parser
        .document()
        .definitions
        .link("late")
        .map(|def| def.dest.as_str());
    assert_eq!(dest, Some("/one"));
}

#[test]
fn leading_blank_lines_empty_trees_and_missing_final_newlines_stay_equivalent() {
    // Leading blank lines: no block lies before the window.
    let mut text = String::from("\n\nbody\n");
    let mut parser = parser_for(&text);
    let body = at(&text, "body");
    let reparse = edit(&mut parser, &mut text, body..body, "x");
    assert_eq!(reparse.blocks_replaced, 0..1);
    assert_eq!(reparse.blocks_inserted, 1);

    // Only blank lines: the tree is empty before and the edit creates the first block.
    let mut text = String::from("\n\n\n");
    let mut parser = parser_for(&text);
    assert!(parser.document().blocks.is_empty());
    let reparse = edit(&mut parser, &mut text, 0..0, "x");
    assert_eq!(reparse.blocks_replaced, 0..0);
    assert_eq!(reparse.blocks_inserted, 1);

    // The last line has no newline: the search for the end of the window runs into the end of
    // the text, both from the line above it and from inside it.
    let mut text = String::from("alpha\nbeta\ngamma");
    let mut parser = parser_for(&text);
    edit(&mut parser, &mut text, 0..0, "x");
    let last = at(&text, "gamma");
    edit(&mut parser, &mut text, last..last, "x");

    // A block that reads a changed label ends in the unterminated last line, and the edit keeps
    // the length of the text.
    let mut text = String::from("[a]: /u\n\nsee [a]");
    let mut parser = parser_for(&text);
    let dest = at(&text, "/u");
    let changed = edit(&mut parser, &mut text, dest + 1..dest + 2, "v");
    assert_eq!(changed.refreshed, vec![1]);
    assert_eq!(changed.blocks_replaced, 0..1);
}

#[test]
fn an_edit_that_keeps_the_front_matter_extent_still_reparses_it() {
    // The closing delimiter is the last line without a newline and the edit does not change the
    // length of the front matter.
    let mut text = String::from("---\nkey: v\n---");
    let mut parser = parser_for(&text);
    let value = at(&text, "v");
    let reparse = edit(&mut parser, &mut text, value..value + 1, "w");
    assert_eq!(reparse.blocks_replaced, 0..1);
    assert_eq!(reparse.blocks_inserted, 1);
    assert!(matches!(
        parser.document().blocks[0].kind,
        BlockKind::FrontMatter { .. }
    ));
}

#[test]
fn a_line_break_typed_inside_a_line_cannot_end_the_window_at_the_rest_of_that_line() {
    // `[foo]: /other` followed by `    indented code` is a definition plus a paragraph that
    // continues it: no blank line separates them, so the indented line is not code. Typing a line
    // break (and a line) before the end of the first line leaves the old line end as an empty line
    // of the new text, which turns the second line into code. Only a blank line that exists in
    // the old text as well proves that the old blocks behind it are unaffected.
    for eol in ["\n", "\r\n"] {
        let mut text =
            format!("para{eol}{eol}[foo]: /other{eol}    indented code{eol}{eol}end{eol}");
        let mut parser = parser_for(&text);
        assert_eq!(parser.document().blocks.len(), 4);
        assert!(matches!(
            parser.document().blocks[2].kind,
            BlockKind::Paragraph
        ));
        let line_end = at(&text, &format!("{eol}    indented"));
        edit(
            &mut parser,
            &mut text,
            line_end..line_end,
            &format!("x{eol}y{eol}"),
        );
        let blocks = &parser.document().blocks;
        assert_eq!(blocks.len(), 5);
        assert!(matches!(blocks[3].kind, BlockKind::CodeBlock(_)));
        assert_eq!(blocks[3].range.get(&text), Some("indented code"));
        // Taking the line break out again restores the paragraph.
        let typed = at(&text, &format!("x{eol}y{eol}"));
        edit(
            &mut parser,
            &mut text,
            typed..typed + 1 + 2 * eol.len() + 1,
            "",
        );
        assert_eq!(parser.document().blocks.len(), 4);
    }
}

#[test]
fn replaying_the_definition_regression_found_by_the_property_test() {
    // The input the property test shrank to: CRLF text whose last blocks are a definition and the
    // paragraph continuing it; a list is typed right behind the definition's destination.
    let mut text = "1. first\n2. second\n$$\nx^2 + y^2\n$$\n\n\n\n- loose\n\n- items\n\n\
                    * [ ] task\n* [x] done\n* [ ] task\n* [x] done\n\n\n\
                    | a | b |\n|---|:-:|\n| 1 | 2 |\n| a | b |\n|---|:-:|\n| 1 | 2 |\n\n\n\
                    [foo]: /other\n    indented code\n\n"
        .replace('\n', "\r\n");
    let mut parser = parser_for(&text);
    assert_eq!(parser.document().blocks.len(), 7);
    let line_end = at(&text, "\r\n    indented code");
    assert_eq!(line_end, 203);
    edit(
        &mut parser,
        &mut text,
        line_end..line_end,
        "1. first\r\n2. second\r\n",
    );
    let blocks = &parser.document().blocks;
    assert_eq!(blocks.len(), 7);
    assert!(matches!(blocks[5].kind, BlockKind::Paragraph));
    assert!(matches!(blocks[6].kind, BlockKind::CodeBlock(_)));
}

// ----------------------------------------------------------------------------- property tests

const FRAGMENTS: &[&str] = &[
    "# Heading\n",
    "## Héading İstanbul ##\n",
    "Plain paragraph with *emphasis*, **strong**, `code` and a [link](http://example.com).\n",
    "Türkçe ığş çö 😀 text\n",
    "- item one\n- item two\n  - nested item\n",
    "- loose\n\n- items\n",
    "1. first\n2. second\n",
    "* [ ] task\n* [x] done\n",
    "> quoted\n> more\n",
    "> > nested quote\n> back\n",
    "> [!NOTE] Title\n> callout body\n",
    "> [!WARNING]\n",
    "```rust\nfn main() {}\n```\n",
    "~~~\ntilde fence\n~~~\n",
    "```\n",
    "~~~\n",
    "| a | b |\n|---|:-:|\n| 1 | 2 |\n",
    "a | b\n--|--\n",
    "$$\nx^2 + y^2\n$$\n",
    "$$x$$\n",
    "$$\n",
    "---\ntitle: Doc\n---\n",
    "+++\nkey = 1\n+++\n",
    "---\n",
    "[foo]: http://example.com \"Title\"\n",
    "[bar]: <http://example.com/a b>\n",
    "[foo]: /other\n",
    "See [foo], [bar][foo] and [baz].\n",
    "[Foo]\n",
    "Note ref[^1] and[^two].\n",
    "[^1]: The note.\n",
    "[^two]: Second\n\n    indented continuation\n",
    "<div>\nraw html\n</div>\n",
    "<!-- comment -->\n",
    "<!-- pagebreak -->\n",
    "***\n",
    "\n",
    "\n\n",
    "    indented code\n",
    "Line one\nLazy line two\n",
    "text\n---\n",
    "![alt|300](img.png)\n",
    "[[wiki|alias]] ==highlight== ~u~ ~~s~~ $m$\n",
];

const SNIPPETS: &[&str] = &[
    "```",
    "~~~",
    "$$",
    "[foo]: url",
    "[x]: url",
    "\n",
    "\n\n",
    "---",
    "+++",
    "> ",
    "- ",
    "    ",
    "#",
    "[^1]: ",
    "[foo]",
    "<div>",
    "<!--",
    "-->",
    "|",
    "é",
    "😀",
    " ",
    "\r\n",
];

const SEPARATORS: &[&str] = &["", "\n", "\n\n"];

fn config(cases: u32) -> ProptestConfig {
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x5357_5249),
        failure_persistence: None,
        ..Config::default()
    }
}

fn document() -> impl Strategy<Value = String> {
    prop::collection::vec(
        (
            prop::sample::select(FRAGMENTS),
            prop::sample::select(SEPARATORS),
        ),
        1..14,
    )
    .prop_map(|parts| {
        let mut text = String::new();
        for (fragment, separator) in parts {
            text.push_str(fragment);
            text.push_str(separator);
        }
        text
    })
}

fn raw_edit() -> impl Strategy<Value = (u8, u32, u32, &'static str)> {
    (
        any::<u8>(),
        any::<u32>(),
        any::<u32>(),
        prop_oneof![
            prop::sample::select(FRAGMENTS),
            prop::sample::select(SNIPPETS)
        ],
    )
}

fn floor_boundary(text: &str, offset: usize) -> usize {
    let mut at = offset.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Turns raw random numbers into a valid `(range, replacement)` for `text`.
fn resolve(text: &str, raw: (u8, u32, u32, &str)) -> (Range<usize>, String) {
    let (kind, position, length, piece) = raw;
    let start = floor_boundary(text, position as usize % (text.len() + 1));
    let end = floor_boundary(text, start + length as usize % 40);
    match kind % 3 {
        0 => (start..start, piece.to_string()),
        1 => (start..end, String::new()),
        _ => (start..end, piece.to_string()),
    }
}

proptest! {
    #![proptest_config(config(2000))]

    #[test]
    fn every_edit_sequence_matches_the_reference_parser(
        text in document(),
        edits in prop::collection::vec(raw_edit(), 1..7),
        crlf in any::<bool>(),
    ) {
        let mut text = if crlf { text.replace('\n', "\r\n") } else { text };
        let mut parser = parser_for(&text);
        for raw in edits {
            let (range, with) = resolve(&text, raw);
            let with = if crlf { with.replace('\n', "\r\n") } else { with };
            edit(&mut parser, &mut text, range, &with);
        }
    }
}

proptest! {
    #![proptest_config(config(500))]

    #[test]
    fn transactions_with_two_edits_match_the_reference_parser(
        text in document(),
        cuts in prop::collection::vec(any::<u32>(), 4),
        pieces in (
            prop_oneof![prop::sample::select(FRAGMENTS), prop::sample::select(SNIPPETS)],
            prop_oneof![prop::sample::select(FRAGMENTS), prop::sample::select(SNIPPETS)],
        ),
    ) {
        let mut offsets: Vec<usize> = cuts
            .iter()
            .map(|&cut| floor_boundary(&text, cut as usize % (text.len() + 1)))
            .collect();
        offsets.sort_unstable();
        let first = Edit::replace(TextRange::new(offsets[0], offsets[1]), pieces.0);
        let second = Edit::replace(TextRange::new(offsets[2], offsets[3]), pieces.1);
        let mut parser = parser_for(&text);
        let mut edited = text.clone();
        edited.replace_range(offsets[2]..offsets[3], pieces.1);
        edited.replace_range(offsets[0]..offsets[1], pieces.0);
        parser.apply(&edited, &[first, second]);
        assert_equivalent(&parser, &edited);
    }
}
