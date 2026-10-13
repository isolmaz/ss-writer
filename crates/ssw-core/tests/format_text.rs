//! The whole-document formatters: JSON with comments, XML and HTML. They are token-preserving
//! whitespace re-emitters, so the tests check what must survive byte for byte (numbers,
//! escapes, duplicate keys, comments, trailing commas, preserved element content, attributes),
//! how the generated whitespace follows each indent setting, that the line ending, the byte
//! order mark and the final newline are kept, that invalid documents are refused untouched and
//! that formatting twice equals formatting once — also for documents with randomly woven gaps.

use ssw_core::format_text::{
    FormatIndent, FormatOutcome, Refusal, RefusalReason, format_html, format_json, format_xml,
};

// ---------------------------------------------------------------------------------- helpers

type Formatter = fn(&str, FormatIndent) -> FormatOutcome;

const INDENTS: [FormatIndent; 3] = [FormatIndent::Two, FormatIndent::Four, FormatIndent::Tab];

/// The text a formatter leaves for `src`; panics on a refusal.
fn run(formatter: Formatter, src: &str, indent: FormatIndent) -> String {
    match formatter(src, indent) {
        FormatOutcome::Formatted(text) => text,
        FormatOutcome::AlreadyFormatted => src.to_string(),
        FormatOutcome::Refused(refusal) => panic!("{src:?} was refused: {refusal}"),
    }
}

fn json(src: &str) -> String {
    run(format_json, src, FormatIndent::Two)
}

fn xml(src: &str) -> String {
    run(format_xml, src, FormatIndent::Two)
}

fn html(src: &str) -> String {
    run(format_html, src, FormatIndent::Two)
}

fn assert_refused(formatter: Formatter, src: &str, reason: RefusalReason, offset: usize) {
    for indent in INDENTS {
        assert_eq!(
            formatter(src, indent),
            FormatOutcome::Refused(Refusal { reason, offset }),
            "{src:?}"
        );
    }
}

/// Rows of `(level, text)` indented with `unit` and joined by `\n` (no final newline).
fn lines(unit: &str, rows: &[(usize, &str)]) -> String {
    rows.iter()
        .map(|(level, row)| format!("{}{row}", unit.repeat(*level)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

/// `src` formats under every indent setting, only whitespace changes, the result is a fixed
/// point, and the indent settings agree on the structure.
fn assert_stable(formatter: Formatter, src: &str) {
    for indent in INDENTS {
        let once = run(formatter, src, indent);
        assert_eq!(
            without_whitespace(&once),
            without_whitespace(src),
            "{src:?} with {indent:?}"
        );
        assert_eq!(
            formatter(&once, indent),
            FormatOutcome::AlreadyFormatted,
            "{src:?} with {indent:?} gave {once:?}"
        );
    }
    let two = run(formatter, src, FormatIndent::Two);
    let four = run(formatter, src, FormatIndent::Four);
    assert_eq!(run(formatter, &four, FormatIndent::Two), two, "{src:?}");
}

/// A tiny deterministic generator (xorshift) for the woven documents.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn step(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[(self.step() % items.len() as u64) as usize]
    }
}

/// `tokens` with a random gap from `gaps` in front of, between and after them.
fn weave(rng: &mut Rng, tokens: &[&str], gaps: &[&str]) -> String {
    let mut out = String::from(rng.pick(gaps));
    for token in tokens {
        out.push_str(token);
        out.push_str(rng.pick(gaps));
    }
    out
}

// ------------------------------------------------------------------------------- refusals

#[test]
fn every_refusal_reason_has_a_short_description() {
    let reasons = [
        (RefusalReason::EmptyDocument, "the document is empty"),
        (RefusalReason::UnexpectedCharacter, "unexpected character"),
        (RefusalReason::UnexpectedToken, "unexpected token"),
        (
            RefusalReason::UnexpectedEnd,
            "the document ends unexpectedly",
        ),
        (
            RefusalReason::TrailingContent,
            "content after the end of the document",
        ),
        (RefusalReason::InvalidNumber, "invalid number"),
        (
            RefusalReason::InvalidEscape,
            "invalid escape sequence in a string",
        ),
        (
            RefusalReason::ControlCharacter,
            "unescaped control character in a string",
        ),
        (RefusalReason::UnterminatedString, "unterminated string"),
        (RefusalReason::UnterminatedComment, "unterminated comment"),
        (RefusalReason::NestingTooDeep, "nesting is too deep"),
        (
            RefusalReason::MismatchedTag,
            "end tag does not match the open element",
        ),
        (RefusalReason::UnclosedTag, "element is never closed"),
        (RefusalReason::MalformedTag, "malformed tag"),
        (
            RefusalReason::UnterminatedAttribute,
            "unterminated attribute value",
        ),
        (
            RefusalReason::UnterminatedConstruct,
            "unterminated markup construct",
        ),
        (
            RefusalReason::ContentOutsideRoot,
            "content outside the root element",
        ),
        (RefusalReason::UnsupportedSyntax, "unsupported syntax"),
    ];
    for (reason, description) in reasons {
        assert_eq!(reason.as_str(), description);
        assert_eq!(reason.to_string(), description);
        // The offset the formatter stopped at follows the description.
        let refusal = Refusal { reason, offset: 12 };
        assert_eq!(refusal.to_string(), format!("{description} (at byte 12)"));
    }
}

// ------------------------------------------------------------------------------------- JSON

#[test]
fn json_keeps_key_order_and_duplicate_keys() {
    let src = r#"{"b":1,"a":2,"b":3,"a":4,"b":{"x":1,"x":2}}"#;
    let expected = lines(
        "  ",
        &[
            (0, "{"),
            (1, r#""b": 1,"#),
            (1, r#""a": 2,"#),
            (1, r#""b": 3,"#),
            (1, r#""a": 4,"#),
            (1, r#""b": {"#),
            (2, r#""x": 1,"#),
            (2, r#""x": 2"#),
            (1, "}"),
            (0, "}"),
        ],
    );
    assert_eq!(json(src), expected);
}

#[test]
fn json_copies_numbers_byte_for_byte() {
    let src = "[1.0,1E5,-0,0.10,123456789012345678901234567890,1e+2,-1.5E-10,0e0,1E400]";
    let expected = lines(
        "  ",
        &[
            (0, "["),
            (1, "1.0,"),
            (1, "1E5,"),
            (1, "-0,"),
            (1, "0.10,"),
            (1, "123456789012345678901234567890,"),
            (1, "1e+2,"),
            (1, "-1.5E-10,"),
            (1, "0e0,"),
            (1, "1E400"),
            (0, "]"),
        ],
    );
    assert_eq!(json(src), expected);
}

#[test]
fn json_copies_strings_and_escapes_byte_for_byte() {
    let value = r##""caf\u00e9 \/ \ud83d\ude00 😀 é \" \\ \n \u00E9""##;
    let src = format!("{{\"k\":{value}}}");
    assert_eq!(json(&src), format!("{{\n  \"k\": {value}\n}}"));
    // Spaces and structural characters inside strings are not whitespace to re-emit.
    let spaced = r#"["  a  ,  b  ","{ [ ] }",":"]"#;
    assert_eq!(
        json(spaced),
        "[\n  \"  a  ,  b  \",\n  \"{ [ ] }\",\n  \":\"\n]"
    );
}

#[test]
fn json_keeps_comments_and_trailing_commas() {
    let src = "// header\n{\n  /* block */\n  \"a\": 1, // trailing\n  \"b\": [1, 2,],\n}\n";
    let expected = "// header\n{\n  /* block */\n  \"a\": 1, // trailing\n  \"b\": [\n    1,\n    2,\n  ],\n}\n";
    assert_eq!(json(src), expected);
    assert_eq!(
        format_json(expected, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
}

#[test]
fn json_places_block_comments_next_to_what_they_belong_to() {
    assert_eq!(
        json(r#"{"a": /* why */ 1, "b": 2 /* tail */}"#),
        "{\n  \"a\": /* why */ 1,\n  \"b\": 2 /* tail */\n}"
    );
    // A comment that leads its entry moves to the entry's line; one that ends the line stays.
    assert_eq!(
        json(r#"{"a":1, /* lead */ "b":2}"#),
        "{\n  \"a\": 1,\n  /* lead */ \"b\": 2\n}"
    );
    assert_eq!(
        json("{\"a\":1, /* tail */\n\"b\":2}"),
        "{\n  \"a\": 1, /* tail */\n  \"b\": 2\n}"
    );
    // A line comment ends its line wherever it sits.
    assert_eq!(json("[ // x\n 1 ]"), "[ // x\n  1\n]");
    assert_eq!(json("[1 // c\n,2]"), "[\n  1 // c\n  ,\n  2\n]");
    assert_eq!(json("{\"a\": // c\n 1}"), "{\n  \"a\": // c\n  1\n}");
}

#[test]
fn json_places_a_run_of_block_comments_by_where_the_run_ends() {
    // The run ends its line — by a line break, a `//` comment or the closing bracket — and
    // stays on the line it started on.
    assert_eq!(json("[/* a */ /* b */\n1]"), "[ /* a */ /* b */\n  1\n]");
    assert_eq!(json("[ /* a */ // c\n 1]"), "[ /* a */ // c\n  1\n]");
    assert_eq!(json("[/* a */ /* b */]"), "[ /* a */ /* b */\n]");
    // The run is followed by its entry on the same line: it moves to the entry's line.
    assert_eq!(json("[ /* a */ /* b */ 1 ]"), "[\n  /* a */ /* b */ 1\n]");
}

#[test]
fn json_keeps_the_comment_content_untouched() {
    let src = "{/*  a\n   b  */\"k\":1//  spaced   \n}";
    let out = json(src);
    assert!(out.contains("/*  a\n   b  */"), "{out:?}");
    assert!(out.contains("//  spaced   \n"), "{out:?}");
}

#[test]
fn json_keeps_empty_containers_collapsed() {
    assert_eq!(json("{}"), "{}");
    assert_eq!(json("[]"), "[]");
    assert_eq!(json("{ }"), "{}");
    assert_eq!(json("[\n\n]"), "[]");
    assert_eq!(
        json(
            r#"{"a":{},"b":[],"c":{ },"d":[
]}"#
        ),
        "{\n  \"a\": {},\n  \"b\": [],\n  \"c\": {},\n  \"d\": []\n}"
    );
    assert_eq!(
        format_json("{}", FormatIndent::Four),
        FormatOutcome::AlreadyFormatted
    );
    // A comment inside expands the container instead of being squeezed against its brackets.
    assert_eq!(json("{ /* only */ }"), "{ /* only */\n}");
    assert_eq!(json("[/*a*/]"), "[ /*a*/\n]");
}

#[test]
fn json_indents_nested_containers_with_every_indent_setting() {
    let src = r#"{"a":{"b":[1,{"c":null}],"d":[]},"e":{}}"#;
    let rows: &[(usize, &str)] = &[
        (0, "{"),
        (1, r#""a": {"#),
        (2, r#""b": ["#),
        (3, "1,"),
        (3, "{"),
        (4, r#""c": null"#),
        (3, "}"),
        (2, "],"),
        (2, r#""d": []"#),
        (1, "},"),
        (1, r#""e": {}"#),
        (0, "}"),
    ];
    for indent in INDENTS {
        assert_eq!(
            run(format_json, src, indent),
            lines(indent.unit(), rows),
            "{indent:?}"
        );
    }
    assert_eq!(FormatIndent::Two.unit(), "  ");
    assert_eq!(FormatIndent::Four.unit(), "    ");
    assert_eq!(FormatIndent::Tab.unit(), "\t");
    assert_eq!(
        run(format_json, "[[1,2],[3]]", FormatIndent::Four),
        lines(
            "    ",
            &[
                (0, "["),
                (1, "["),
                (2, "1,"),
                (2, "2"),
                (1, "],"),
                (1, "["),
                (2, "3"),
                (1, "]"),
                (0, "]"),
            ]
        )
    );
}

#[test]
fn json_puts_every_array_element_on_its_own_line() {
    assert_eq!(json("[1,2,3]"), "[\n  1,\n  2,\n  3\n]");
    assert_eq!(json("[\"a\" ,\n \"b\"]"), "[\n  \"a\",\n  \"b\"\n]");
    assert_eq!(json("42"), "42");
    assert_eq!(json("  \"s\"  "), "\"s\"");
    assert_eq!(json("null"), "null");
}

#[test]
fn json_keeps_at_most_one_blank_line_between_entries() {
    assert_eq!(
        json("{\n\n  \"a\": 1,\n\n\n\n  \"b\": 2\n\n}"),
        "{\n  \"a\": 1,\n\n  \"b\": 2\n}"
    );
    let commented = "{\n  // a\n\n  // b\n  \"x\": 1\n}";
    assert_eq!(
        format_json(commented, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
}

#[test]
fn json_keeps_crlf_the_byte_order_mark_and_the_final_newline_state() {
    assert_eq!(
        json("{\r\n\"a\":1,\r\n\"b\":[1,2]\r\n}\r\n"),
        "{\r\n  \"a\": 1,\r\n  \"b\": [\r\n    1,\r\n    2\r\n  ]\r\n}\r\n"
    );
    // Only the final line break is CRLF: the generated ones follow it.
    assert_eq!(json("{\"a\":1}\r\n"), "{\r\n  \"a\": 1\r\n}\r\n");
    // LF stays LF, and the dominant spelling wins in a mixed document.
    assert_eq!(json("[1,\n2]\n"), "[\n  1,\n  2\n]\n");
    assert_eq!(json("[1,\r\n2,\r\n3\n]"), "[\r\n  1,\r\n  2,\r\n  3\r\n]");
    // A byte order mark stays in front.
    assert_eq!(json("\u{feff}{\"a\":1}\n"), "\u{feff}{\n  \"a\": 1\n}\n");
    assert_eq!(json("\u{feff}[]"), "\u{feff}[]");
    // No final newline stays none; several collapse to one; trailing spaces go.
    assert_eq!(json("[1]"), "[\n  1\n]");
    assert_eq!(json("[1]\n"), "[\n  1\n]\n");
    assert_eq!(json("[1]\n\n\n"), "[\n  1\n]\n");
    assert_eq!(json("[1]   "), "[\n  1\n]");
    assert_eq!(json("[1] // end"), "[\n  1\n] // end");
}

#[test]
fn json_keeps_a_lone_cr_line_ending_and_picks_the_dominant_spelling() {
    // A document whose line breaks are lone CRs gets lone CRs, and keeps its final one.
    assert_eq!(
        json("{\r\"a\":1,\r\"b\":2\r}\r"),
        "{\r  \"a\": 1,\r  \"b\": 2\r}\r"
    );
    // CR is the most frequent spelling here, two against one CRLF and no LF.
    assert_eq!(json("[1,\r2,\r3\r\n]"), "[\r  1,\r  2,\r  3\r]");
    // On a tie LF wins over both, and CRLF over a lone CR.
    assert_eq!(json("[1,\n2,\r3]"), "[\n  1,\n  2,\n  3\n]");
    assert_eq!(json("[1,\r2,\r\n3]"), "[\r\n  1,\r\n  2,\r\n  3\r\n]");
}

#[test]
fn json_refuses_documents_that_do_not_parse_and_names_the_offset() {
    let cases: &[(&str, RefusalReason, usize)] = &[
        (r#"{"a":1"#, RefusalReason::UnexpectedEnd, 6),
        (r#"{"a":1 "b":2}"#, RefusalReason::UnexpectedToken, 7),
        ("{a:1}", RefusalReason::UnexpectedCharacter, 1),
        ("[1,2", RefusalReason::UnexpectedEnd, 4),
        (r#"{"a":1}{"b":2}"#, RefusalReason::TrailingContent, 7),
        ("[01]", RefusalReason::UnexpectedToken, 2),
        ("[1.]", RefusalReason::InvalidNumber, 3),
        ("[1e]", RefusalReason::InvalidNumber, 3),
        ("[-]", RefusalReason::InvalidNumber, 2),
        (r#"["abc"#, RefusalReason::UnterminatedString, 1),
        // The document ends right after the backslash that starts an escape.
        (r#"["abc\"#, RefusalReason::UnterminatedString, 1),
        (r#"["a\qb"]"#, RefusalReason::InvalidEscape, 3),
        (r#"["a\u12g4"]"#, RefusalReason::InvalidEscape, 3),
        ("[\"a\tb\"]", RefusalReason::ControlCharacter, 3),
        ("/* never closed", RefusalReason::UnterminatedComment, 0),
        (r#"{"a":1,,}"#, RefusalReason::UnexpectedToken, 7),
        ("[1 2]", RefusalReason::UnexpectedToken, 3),
        (r#"["a":1]"#, RefusalReason::UnexpectedToken, 4),
        (r#"{"a":1]"#, RefusalReason::UnexpectedToken, 6),
        (r#"{"a":}"#, RefusalReason::UnexpectedToken, 5),
        ("{1:2}", RefusalReason::UnexpectedToken, 1),
        ("+1", RefusalReason::UnexpectedCharacter, 0),
        ("NaN", RefusalReason::UnexpectedCharacter, 0),
        ("'x'", RefusalReason::UnexpectedCharacter, 0),
        ("tru", RefusalReason::UnexpectedCharacter, 0),
        ("[1] / 2", RefusalReason::UnexpectedCharacter, 4),
        ("  \n", RefusalReason::EmptyDocument, 3),
        ("// only a comment", RefusalReason::EmptyDocument, 17),
    ];
    for &(src, reason, offset) in cases {
        assert_refused(format_json, src, reason, offset);
    }
}

#[test]
fn a_missing_closing_brace_is_refused_and_nothing_is_offered() {
    let src = "{\n  \"a\": [1, 2],\n  \"b\": {\"c\": 3}\n";
    let outcome = format_json(src, FormatIndent::Two);
    let FormatOutcome::Refused(refusal) = outcome else {
        panic!("not refused: {outcome:?}");
    };
    assert_eq!(refusal.reason, RefusalReason::UnexpectedEnd);
    assert_eq!(refusal.offset, src.len());
    assert_eq!(
        refusal.to_string(),
        format!("the document ends unexpectedly (at byte {})", src.len())
    );
}

#[test]
fn json_refuses_nesting_deeper_than_it_supports_but_formats_the_limit() {
    assert_refused(
        format_json,
        &"[".repeat(600),
        RefusalReason::NestingTooDeep,
        512,
    );
    let limit = format!("{}{}", "[".repeat(512), "]".repeat(512));
    assert!(matches!(
        format_json(&limit, FormatIndent::Two),
        FormatOutcome::Formatted(_)
    ));
}

#[test]
fn json_reports_unchanged_formatted_and_refused_documents_apart() {
    assert_eq!(
        format_json("[1]", FormatIndent::Two),
        FormatOutcome::Formatted("[\n  1\n]".to_string())
    );
    assert_eq!(
        format_json("[\n  1\n]", FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
    assert_eq!(
        format_json("[\n  1\n]", FormatIndent::Four),
        FormatOutcome::Formatted("[\n    1\n]".to_string())
    );
    assert!(matches!(
        format_json("[1", FormatIndent::Two),
        FormatOutcome::Refused(_)
    ));
}

#[test]
fn json_formatting_twice_equals_formatting_once() {
    let docs = [
        r#"{"a":{"b":[1,{"c":null}],"d":[]},"e":{}}"#,
        "// c\n{ /* only */ }\n",
        "[/*a*/]",
        "[1, // a\n // b\n 2]",
        "{\"a\":1, /* lead */ \"b\":2}",
        "[ // x\n 1 ]",
        "[1 // c\n,2]",
        "{\"a\": // c\n 1}",
        "{\n\n  // a\n\n\n  // b\n  \"x\": [1,\n\n2,],\n}\r\n",
        "\u{feff}{\"k\" /* c */ : /* d */ [ ] }",
        "[1,2,3] // end",
        "[ /* a */ /* b */ 1 ]",
        "[/* a */ /* b */\n1]",
        "[ /* a */ // c\n 1]",
        "[/* a */ /* b */]",
        "{\r\"a\":1,\r\"b\":2\r}\r",
        "[1,\r2,\r3\r\n]",
    ];
    for doc in docs {
        assert_stable(format_json, doc);
    }
}

const JSON_TOKENS: &[&str] = &[
    "{", "\"a\"", ":", "[", "1", ",", "2.5e3", ",", "{", "\"b\"", ":", "null", ",", "\"c\"", ":",
    "[", "]", "}", ",", "\"x\"", ",", "true", "]", ",", "\"d\"", ":", "{", "}", ",", "\"e\"", ":",
    "\"s\"", ",", "}",
];

const JSON_GAPS: &[&str] = &[
    "",
    "",
    " ",
    "  ",
    "\t",
    "\n",
    "\r\n",
    "\n\n",
    "\n  \n",
    " /* c */",
    "/* c */ ",
    "\n/* multi\nline */\n",
    " // line\n",
    "\n// own\n",
    "\n\n// own\n\n",
];

#[test]
fn json_with_randomly_woven_gaps_formats_stably() {
    for seed in 1..=300 {
        let src = weave(&mut Rng::new(seed), JSON_TOKENS, JSON_GAPS);
        assert_stable(format_json, &src);
    }
}

// ------------------------------------------------------------------------------------- XML

#[test]
fn xml_indents_nested_elements_with_every_indent_setting() {
    let src = "<root><a><b>text</b></a><c/></root>\n";
    let rows: &[(usize, &str)] = &[
        (0, "<root>"),
        (1, "<a>"),
        (2, "<b>text</b>"),
        (1, "</a>"),
        (1, "<c/>"),
        (0, "</root>"),
    ];
    for indent in INDENTS {
        assert_eq!(
            run(format_xml, src, indent),
            lines(indent.unit(), rows) + "\n",
            "{indent:?}"
        );
    }
}

#[test]
fn xml_reindents_an_already_indented_document_and_keeps_one_blank_line() {
    let src = "<a>\n      <b/>\n\n\n\n   <c>\n<d/>\n  </c>\n</a>";
    assert_eq!(
        run(format_xml, src, FormatIndent::Four),
        "<a>\n    <b/>\n\n    <c>\n        <d/>\n    </c>\n</a>"
    );
}

#[test]
fn xml_keeps_mixed_content_verbatim() {
    let src = "<doc>\n<p>Hello <b>big</b>   world,\n   again</p>\n<list><i>1</i><i>2</i></list>\n</doc>\n";
    assert_eq!(
        xml(src),
        "<doc>\n  <p>Hello <b>big</b>   world,\n   again</p>\n  <list>\n    <i>1</i>\n    <i>2</i>\n  </list>\n</doc>\n"
    );
    // A text node next to an element makes the whole element verbatim, including the
    // whitespace around the child elements.
    let mixed = "<r>\n   <a/>  tail\n   <b/>\n</r>\n";
    assert_eq!(
        format_xml(mixed, FormatIndent::Four),
        FormatOutcome::AlreadyFormatted
    );
    // So does an entity reference: it is text.
    let entity = "<r>\n<a/>&#10;<b/>\n</r>\n";
    assert_eq!(
        format_xml(entity, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
}

#[test]
fn xml_treats_whitespace_without_a_line_break_between_children_as_data() {
    let src = "<r><a>x</a> <b>y</b></r>\n";
    assert_eq!(
        format_xml(src, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
    let tabbed = "<r>\n<a/>\t<b/>\n</r>\n";
    assert_eq!(
        format_xml(tabbed, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
}

#[test]
fn xml_keeps_the_children_of_svg_text_and_xaml_text_elements_touching() {
    // `<tspan>a</tspan><tspan>b</tspan>` reads `ab`; a line break between the two would make it
    // `a b`. Generic XML cannot know that, so the elements that render inline are listed.
    let svg = "<svg><g><text><tspan>a</tspan><tspan>b</tspan></text></g></svg>";
    assert_eq!(
        xml(svg),
        "<svg>\n  <g>\n    <text><tspan>a</tspan><tspan>b</tspan></text>\n  </g>\n</svg>"
    );
    let xaml = "<StackPanel><TextBlock><Run>Hello</Run><Run>World</Run></TextBlock></StackPanel>";
    assert_eq!(
        xml(xaml),
        "<StackPanel>\n  <TextBlock><Run>Hello</Run><Run>World</Run></TextBlock>\n</StackPanel>"
    );
    // Also as the root element.
    for doc in [
        "<text><tspan>a</tspan><tspan>b</tspan></text>",
        "<TextBlock><Run>Hello</Run><Run>World</Run></TextBlock>\n",
    ] {
        assert_eq!(
            format_xml(doc, FormatIndent::Four),
            FormatOutcome::AlreadyFormatted,
            "{doc:?}"
        );
    }
    // Every listed name keeps its children byte for byte, with or without whitespace between.
    for name in [
        "text",
        "tspan",
        "textPath",
        "title",
        "desc",
        "TextBlock",
        "Paragraph",
        "Span",
        "Run",
        "Bold",
        "Italic",
        "Underline",
        "Hyperlink",
    ] {
        let touching = format!("<r><{name}><x>a</x><x>b</x></{name}></r>");
        assert_eq!(
            xml(&touching),
            format!("<r>\n  <{name}><x>a</x><x>b</x></{name}>\n</r>"),
            "{name}"
        );
        let broken = format!("<r><{name}><x>a</x>\n      <x>b</x></{name}></r>");
        assert_eq!(
            xml(&broken),
            format!("<r>\n  <{name}><x>a</x>\n      <x>b</x></{name}>\n</r>"),
            "{name}"
        );
    }
}

#[test]
fn xml_lays_out_the_children_of_every_other_element_as_structure() {
    // The limitation: only the listed names are known to render their children inline.
    assert_eq!(
        xml("<r><p><x>a</x><x>b</x></p></r>"),
        "<r>\n  <p>\n    <x>a</x>\n    <x>b</x>\n  </p>\n</r>"
    );
    // Names are case-sensitive, as XML is.
    assert_eq!(
        xml("<r><Text><x>a</x><x>b</x></Text></r>"),
        "<r>\n  <Text>\n    <x>a</x>\n    <x>b</x>\n  </Text>\n</r>"
    );
    // A namespace prefix does not hide a listed name.
    let prefixed = concat!(
        "<svg:svg xmlns:svg=\"http://www.w3.org/2000/svg\">",
        "<svg:text><svg:tspan>a</svg:tspan><svg:tspan>b</svg:tspan></svg:text></svg:svg>",
    );
    assert_eq!(
        xml(prefixed),
        concat!(
            "<svg:svg xmlns:svg=\"http://www.w3.org/2000/svg\">\n",
            "  <svg:text><svg:tspan>a</svg:tspan><svg:tspan>b</svg:tspan></svg:text>\n",
            "</svg:svg>",
        )
    );
}

#[test]
fn xml_keeps_an_element_with_a_style_that_sets_display_or_white_space_verbatim() {
    // XML knows no CSS, but an inline `style` still says the content is laid out by something
    // else; the element stays a structural child of its parent.
    for style in ["display:none", "white-space: pre"] {
        let src = format!("<root><g style=\"{style}\"><a/>  <b/></g><h><a/></h></root>");
        assert_eq!(
            xml(&src),
            format!(
                "<root>\n  <g style=\"{style}\"><a/>  <b/></g>\n  <h>\n    <a/>\n  </h>\n</root>"
            ),
            "{style}"
        );
    }
}

#[test]
fn xml_keeps_a_lone_cr_line_ending() {
    assert_eq!(xml("<a>\r<b/>\r</a>\r"), "<a>\r  <b/>\r</a>\r");
    // CR is the most frequent spelling here, two against one CRLF and no LF.
    assert_eq!(xml("<a>\r<b/>\r\n<c/>\r</a>"), "<a>\r  <b/>\r  <c/>\r</a>");
}

#[test]
fn xml_keeps_cdata_comments_and_processing_instructions_untouched() {
    let src = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!-- top\n     comment -->\n",
        "<root>\n",
        "<!--   spaced   comment   -->\n",
        "<?pi   data   ?>\n",
        "<item><![CDATA[ <raw>   & text ]]></item>\n",
        "<item>a</item>\n",
        "</root>\n",
    );
    let expected = concat!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n",
        "<!-- top\n     comment -->\n",
        "<root>\n",
        "  <!--   spaced   comment   -->\n",
        "  <?pi   data   ?>\n",
        "  <item><![CDATA[ <raw>   & text ]]></item>\n",
        "  <item>a</item>\n",
        "</root>\n",
    );
    assert_eq!(xml(src), expected);
    // CDATA beside an element is mixed content.
    assert_eq!(
        xml("<doc><mix><![CDATA[x]]><b/></mix></doc>\n"),
        "<doc>\n  <mix><![CDATA[x]]><b/></mix>\n</doc>\n"
    );
    // A `--` that does not end the comment is part of its text; `--!>` ends only an HTML one.
    assert_eq!(
        xml("<a><!-- x -- y --!> z --><b/></a>"),
        "<a>\n  <!-- x -- y --!> z -->\n  <b/>\n</a>"
    );
}

#[test]
fn xml_keeps_preserved_content_untouched() {
    let src = "<root><keep xml:space=\"preserve\"><a>  x </a><b/></keep><free><a/></free></root>\n";
    assert_eq!(
        xml(src),
        "<root>\n  <keep xml:space=\"preserve\"><a>  x </a><b/></keep>\n  <free>\n    <a/>\n  </free>\n</root>\n"
    );
    let named =
        "<doc><pre>  a\n   b </pre><script>x  =  1;\n  y</script><style>b {  c: d  }</style></doc>";
    assert_eq!(
        xml(named),
        "<doc>\n  <pre>  a\n   b </pre>\n  <script>x  =  1;\n  y</script>\n  <style>b {  c: d  }</style>\n</doc>"
    );
}

#[test]
fn xml_keeps_attributes_in_their_spelling_and_order() {
    let src = "<root>\n<item  id = \"1\"   name='x'   data-x=\"a  b\"/>\n<other a=\"1\" b=\"2\">\n<c/>\n</other>\n</root>\n";
    assert_eq!(
        xml(src),
        "<root>\n  <item  id = \"1\"   name='x'   data-x=\"a  b\"/>\n  <other a=\"1\" b=\"2\">\n    <c/>\n  </other>\n</root>\n"
    );
    // Empty-element syntax is never converted either way.
    assert_eq!(
        xml("<r><a></a><b/><c> </c></r>"),
        "<r>\n  <a></a>\n  <b/>\n  <c> </c>\n</r>"
    );
}

#[test]
fn xml_keeps_the_declaration_first_and_normalizes_the_gaps_around_the_prolog() {
    assert_eq!(
        xml("\n\n<?xml version=\"1.0\"?>\n\n\n<a><b/></a>\n\n\n"),
        "<?xml version=\"1.0\"?>\n\n<a>\n  <b/>\n</a>\n"
    );
    assert_eq!(
        xml("<?xml version=\"1.0\"?><a><b/></a>"),
        "<?xml version=\"1.0\"?>\n<a>\n  <b/>\n</a>"
    );
}

#[test]
fn xml_keeps_a_root_with_an_internal_dtd_subset_verbatim() {
    let src = concat!(
        "<?xml version=\"1.0\"?>\n",
        "<!DOCTYPE note [\n",
        "<!ENTITY a \"x > y\">\n",
        "<!-- ] > -->\n",
        "<!ATTLIST pre xml:space (default|preserve) 'preserve'>\n",
        "]>\n",
        "<note>\n<to>T</to>\n</note>\n",
    );
    assert_eq!(
        format_xml(src, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
}

#[test]
fn xml_keeps_crlf_the_byte_order_mark_and_the_final_newline_state() {
    assert_eq!(
        run(format_xml, "<a>\r\n<b/>\r\n</a>\r\n", FormatIndent::Four),
        "<a>\r\n    <b/>\r\n</a>\r\n"
    );
    assert_eq!(xml("\u{feff}<a><b/></a>"), "\u{feff}<a>\n  <b/>\n</a>");
    assert_eq!(xml("<a><b/></a>\n\n\n"), "<a>\n  <b/>\n</a>\n");
    // Line breaks inside preserved content keep their spelling.
    assert_eq!(
        xml("<a>\r\n<pre>x\ny</pre>\r\n</a>"),
        "<a>\r\n  <pre>x\ny</pre>\r\n</a>"
    );
}

#[test]
fn xml_refuses_malformed_documents_and_names_the_offset() {
    let cases: &[(&str, RefusalReason, usize)] = &[
        ("<a><b></a>", RefusalReason::MismatchedTag, 6),
        ("<a><b></b>", RefusalReason::UnclosedTag, 0),
        ("</a>", RefusalReason::MismatchedTag, 0),
        ("<a/><b/>", RefusalReason::ContentOutsideRoot, 4),
        ("text<a/>", RefusalReason::ContentOutsideRoot, 0),
        ("<a><!-- x </a>", RefusalReason::UnterminatedComment, 3),
        (
            "<a href=\"x>text</a>",
            RefusalReason::UnterminatedAttribute,
            8,
        ),
        // Attributes need a value, quotes around it and whitespace in front.
        ("<a b>", RefusalReason::MalformedTag, 4),
        ("<a b=c/>", RefusalReason::MalformedTag, 5),
        ("<a b=\"1\"c=\"2\"/>", RefusalReason::MalformedTag, 8),
        ("<a 1=\"x\"/>", RefusalReason::MalformedTag, 3),
        ("<a / >", RefusalReason::MalformedTag, 3),
        ("<a b=", RefusalReason::UnexpectedEnd, 5),
        ("<a b= ", RefusalReason::UnexpectedEnd, 6),
        ("<a><![CDATA[x</a>", RefusalReason::UnterminatedConstruct, 3),
        ("<a><?pi</a>", RefusalReason::UnterminatedConstruct, 3),
        ("<a", RefusalReason::UnexpectedEnd, 2),
        ("<>", RefusalReason::MalformedTag, 0),
        // End tags without a name, with a slash, or that the document ends in.
        ("<a></a", RefusalReason::UnexpectedEnd, 6),
        ("<a></></a>", RefusalReason::MalformedTag, 3),
        ("<a></a/>", RefusalReason::MalformedTag, 3),
        // XML has no declaration but the DOCTYPE, and that only before the root element.
        ("<a><!FOO></a>", RefusalReason::UnsupportedSyntax, 3),
        ("<a><!DOCTYPE a></a>", RefusalReason::UnsupportedSyntax, 3),
        ("<!DOCTYPE a", RefusalReason::UnterminatedConstruct, 0),
        (
            "<!DOCTYPE a [<!ENTITY x \"y\">",
            RefusalReason::UnterminatedConstruct,
            0,
        ),
        (
            "<!DOCTYPE a [<!-- x ]>",
            RefusalReason::UnterminatedConstruct,
            0,
        ),
        ("", RefusalReason::EmptyDocument, 0),
        (
            "<?xml version=\"1.0\"?>\n",
            RefusalReason::EmptyDocument,
            22,
        ),
    ];
    for &(src, reason, offset) in cases {
        assert_refused(format_xml, src, reason, offset);
    }
    let deep = "<a>".repeat(300);
    assert_refused(format_xml, &deep, RefusalReason::NestingTooDeep, 768);
    let limit = format!("{}{}", "<a>".repeat(256), "</a>".repeat(256));
    assert!(matches!(
        format_xml(&limit, FormatIndent::Two),
        FormatOutcome::Formatted(_)
    ));
}

#[test]
fn xml_formatting_twice_equals_formatting_once() {
    let docs = [
        "<root><a><b>text</b></a><c/></root>\n",
        "<doc>\n<p>Hello <b>big</b> world</p>\n<list><i>1</i><i>2</i></list>\n</doc>\n",
        "<r><a>x</a> <b>y</b></r>",
        "<?xml version=\"1.0\"?>\n<!-- c -->\n<root>\n<?pi x?>\n<a/>\n\n\n<b><![CDATA[x]]></b>\n</root>\n",
        "<root><keep xml:space=\"preserve\"><a>  x </a></keep><free><a/></free></root>",
        "<a>\r\n\r\n<b/>\r\n</a>\r\n",
        "<svg><g><text><tspan>a</tspan><tspan>b</tspan></text></g></svg>",
        "<StackPanel><TextBlock><Run>Hello</Run><Run>World</Run></TextBlock></StackPanel>\n",
        "<root><g style=\"display:none\"><a/>  <b/></g><h><a/></h></root>",
        "<a>\r<b/>\r</a>\r",
        "<a><!-- x -- y --!> z --><b/></a>",
    ];
    for doc in docs {
        assert_stable(format_xml, doc);
    }
}

const XML_NODES: &[&str] = &[
    "<?xml version=\"1.0\"?>",
    "<!-- head -->",
    "<root a=\"1\" b='2'>",
    "<a>",
    "<b>x</b>",
    "<?pi data?>",
    "<c/>",
    "</a>",
    "<d>",
    "<![CDATA[ <e> ]]>",
    "</d>",
    "<f>",
    "<g>1</g>",
    "<g>2</g>",
    "</f>",
    "<m>t<n/>u</m>",
    "</root>",
];

const MARKUP_GAPS: &[&str] = &[
    "", "", " ", "\n", "\r\n", "\n\n\n", "  \n  ", "\t", "\n    ",
];

#[test]
fn xml_with_randomly_woven_gaps_formats_stably() {
    for seed in 1..=300 {
        let src = weave(&mut Rng::new(seed), XML_NODES, MARKUP_GAPS);
        assert_stable(format_xml, &src);
    }
}

// ------------------------------------------------------------------------------------ HTML

#[test]
fn html_indents_nested_block_elements_with_every_indent_setting() {
    let src = "<!DOCTYPE html><html><head><title>T</title></head><body><div><p>a</p><ul><li>x</li></ul></div></body></html>\n";
    let rows: &[(usize, &str)] = &[
        (0, "<!DOCTYPE html>"),
        (0, "<html>"),
        (1, "<head>"),
        (2, "<title>T</title>"),
        (1, "</head>"),
        (1, "<body>"),
        (2, "<div>"),
        (3, "<p>a</p>"),
        (3, "<ul>"),
        (4, "<li>x</li>"),
        (3, "</ul>"),
        (2, "</div>"),
        (1, "</body>"),
        (0, "</html>"),
    ];
    for indent in INDENTS {
        let unit = indent.unit();
        // The DOCTYPE and `<html>` both start at column 0.
        assert_eq!(
            run(format_html, src, indent),
            lines(unit, rows) + "\n",
            "{indent:?}"
        );
    }
}

#[test]
fn html_keeps_pre_and_textarea_content_untouched() {
    let src = "<div>\n<pre>  a\n    b  </pre>\n<section><textarea>\n raw\n</textarea></section>\n</div>\n";
    assert_eq!(
        html(src),
        "<div>\n  <pre>  a\n    b  </pre>\n  <section><textarea>\n raw\n</textarea></section>\n</div>\n"
    );
    let nested = "<div><pre>\n  x\n  <b>  y  </b>\n</pre></div>";
    assert_eq!(
        html(nested),
        "<div>\n  <pre>\n  x\n  <b>  y  </b>\n</pre>\n</div>"
    );
}

#[test]
fn html_keeps_script_and_style_bodies_untouched() {
    let src = concat!(
        "<html>\n<head>\n",
        "<style>\n  body { color: red }\n</style>\n",
        "<script>\nif (a < b && c > d) { document.write(\"</div>\"); }\n</script>\n",
        "</head>\n<body>\n<p>text</p>\n</body>\n</html>\n",
    );
    let expected = concat!(
        "<html>\n  <head>\n",
        "    <style>\n  body { color: red }\n</style>\n",
        "    <script>\nif (a < b && c > d) { document.write(\"</div>\"); }\n</script>\n",
        "  </head>\n  <body>\n    <p>text</p>\n  </body>\n</html>\n",
    );
    assert_eq!(html(src), expected);
    // The end tag is found case-insensitively, after other `</` sequences.
    let upper = "<head><SCRIPT>a</b>  b</SCRIPT ><style>  x  </STYLE></head>";
    assert_eq!(
        html(upper),
        "<head>\n  <SCRIPT>a</b>  b</SCRIPT >\n  <style>  x  </STYLE>\n</head>"
    );
}

#[test]
fn html_keeps_comments_and_conditional_comments_untouched() {
    let src = "<head>\n<!--[if IE]><p>ie</p><![endif]-->\n<meta charset=\"utf-8\">\n<!--   a\n   b   -->\n</head>\n";
    assert_eq!(
        html(src),
        "<head>\n  <!--[if IE]><p>ie</p><![endif]-->\n  <meta charset=\"utf-8\">\n  <!--   a\n   b   -->\n</head>\n"
    );
    // Odd comment forms end where the HTML tokenizer ends them.
    let odd = "<div><!--><!--->\n<!-- a --!>\n<?php echo 1; ?>\n<p>x</p></div>";
    assert_eq!(
        html(odd),
        "<div>\n  <!-->\n  <!--->\n  <!-- a --!>\n  <?php echo 1; ?>\n  <p>x</p>\n</div>"
    );
    // A `--` or `--!` that does not end the comment is part of its text.
    assert_eq!(
        html("<div><!-- a -- b --! c --><p>x</p></div>"),
        "<div>\n  <!-- a -- b --! c -->\n  <p>x</p>\n</div>"
    );
}

#[test]
fn html_reads_cdata_declarations_and_odd_end_tags_as_the_tokenizer_does() {
    // CDATA is a section only in SVG and MathML; anywhere else it is a bogus comment that ends
    // at the first `>`, so what follows that `>` is text.
    assert_eq!(
        html("<div><![CDATA[x]]><p>a</p></div>"),
        "<div>\n  <![CDATA[x]]>\n  <p>a</p>\n</div>"
    );
    for doc in [
        "<div><![CDATA[ a > b ]]><p>x</p></div>",
        "<div><svg><![CDATA[ a > b ]]></svg></div>",
    ] {
        assert_eq!(
            format_html(doc, FormatIndent::Two),
            FormatOutcome::AlreadyFormatted,
            "{doc:?}"
        );
    }
    // Any other `<!…>` is a bogus comment, and so is an end tag without a letter in front.
    assert_eq!(
        html("<div><!foo bar><p>a</p></div>"),
        "<div>\n  <!foo bar>\n  <p>a</p>\n</div>"
    );
    assert_eq!(
        html("<div></3 ><p>a</p></div>"),
        "<div>\n  </3 >\n  <p>a</p>\n</div>"
    );
    assert_eq!(html("<div></></div>"), "<div>\n  </>\n</div>");
    // A `<` that no tag, comment or declaration follows is text.
    for doc in ["<", "1 <", "a </", "a < b", "<3"] {
        assert_eq!(
            format_html(doc, FormatIndent::Two),
            FormatOutcome::AlreadyFormatted,
            "{doc:?}"
        );
    }
}

#[test]
fn html_follows_the_tokenizer_for_stray_slashes_and_raw_text_end_tags() {
    // A slash inside a tag that does not end it is ignored.
    assert_eq!(
        html("<div><p / id=\"a\">x</p></div>"),
        "<div>\n  <p / id=\"a\">x</p>\n</div>"
    );
    // `</script` ends the script only when a space, a slash or `>` follows it.
    assert_eq!(
        html("<div><script>a</scriptx> b</script></div>"),
        "<div>\n  <script>a</scriptx> b</script>\n</div>"
    );
    // Only a script has a double-escaped state.
    assert_eq!(
        html("<head><style><!--<script></style></head>"),
        "<head>\n  <style><!--<script></style>\n</head>"
    );
}

#[test]
fn html_keeps_a_lone_cr_line_ending() {
    assert_eq!(
        html("<div>\r<p>a</p>\r</div>\r"),
        "<div>\r  <p>a</p>\r</div>\r"
    );
}

#[test]
fn html_leaves_a_container_with_inline_content_verbatim() {
    let src = "<body>\n<p>Hello <b>big</b>   world</p>\n<div>\n<span>a</span> <span>b</span>\n</div>\n</body>\n";
    assert_eq!(
        html(src),
        "<body>\n  <p>Hello <b>big</b>   world</p>\n  <div>\n<span>a</span> <span>b</span>\n</div>\n</body>\n"
    );
    // Whitespace between inline elements is rendered: nothing may be added or removed.
    for doc in [
        "<p><b>a</b> <i>b</i></p>\n",
        "<p><b>a</b><i>b</i></p>\n",
        "<div>\n<a href=\"x\">x</a>\n</div>\n",
        "<li><a href=\"x\">x</a></li>\n",
        "text and <b>inline</b>\n",
        "<custom-element>\n<p>x</p>\n</custom-element>\n",
        "<div>\n<p>x</p>\n<my-tag>y</my-tag>\n</div>\n",
    ] {
        assert_eq!(
            format_html(doc, FormatIndent::Four),
            FormatOutcome::AlreadyFormatted,
            "{doc:?}"
        );
    }
}

#[test]
fn html_leaves_svg_and_styled_elements_verbatim() {
    let svg =
        "<body>\n<svg viewBox=\"0 0 1 1\"><text>  a  </text><path d=\"M0 0\"/></svg>\n</body>\n";
    assert_eq!(
        format_html(svg, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
    // `white-space` keeps the content of the element as it is; the element stays a block for
    // its parent, which is laid out around it.
    let styled = "<div>\n<section style=\"white-space: pre-line\">\n<p>a</p>\n</section>\n<section style=\"color: red\">\n<p>b</p>\n</section>\n</div>\n";
    assert_eq!(
        html(styled),
        "<div>\n  <section style=\"white-space: pre-line\">\n<p>a</p>\n</section>\n  <section style=\"color: red\">\n    <p>b</p>\n  </section>\n</div>\n"
    );
    let template = "<div><template><p>  x  </p></template><p>y</p></div>";
    assert_eq!(
        html(template),
        "<div>\n  <template><p>  x  </p></template>\n  <p>y</p>\n</div>"
    );
}

#[test]
fn html_does_not_split_the_siblings_of_an_element_that_sets_its_display() {
    // `display` may make the element inline-level, and whitespace between inline elements is
    // rendered: a line break between the two `li` would put a gap between `a` and `b`.
    for display in ["inline", "inline-block", "inline-flex"] {
        let list = format!(
            "<ul><li style=\"display:{display}\">a</li><li style=\"display:{display}\">b</li></ul>"
        );
        assert_eq!(
            format_html(&list, FormatIndent::Two),
            FormatOutcome::AlreadyFormatted,
            "{display}"
        );
        // The parent that has other block children is laid out around the list.
        let nested = format!("<div>{list}<p>x</p></div>");
        assert_eq!(
            html(&nested),
            format!("<div>\n  {list}\n  <p>x</p>\n</div>"),
            "{display}"
        );
    }
    // The style is found in any case, and among other declarations.
    for style in ["DISPLAY: Inline", "color: red; display : inline-block"] {
        let src = format!("<ul><li style=\"{style}\">a</li><li>b</li></ul>");
        assert_eq!(
            format_html(&src, FormatIndent::Two),
            FormatOutcome::AlreadyFormatted,
            "{style}"
        );
    }
    // A single element with a display of its own stays as it is, with its content.
    let flex = "<section style=\"display: flex\">\n<p>a</p>\n</section>\n";
    assert_eq!(
        format_html(flex, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
    // Without `display` the same list is structure.
    assert_eq!(
        html("<ul><li style=\"color: red\">a</li><li>b</li></ul>"),
        "<ul>\n  <li style=\"color: red\">a</li>\n  <li>b</li>\n</ul>"
    );
}

#[test]
fn html_accepts_void_and_self_closing_elements() {
    let src = "<head>\n<meta charset=\"utf-8\">\n<link rel=\"stylesheet\" href=\"a.css\"/>\n<title>Doc</title>\n</head>\n";
    assert_eq!(
        run(format_html, src, FormatIndent::Four),
        "<head>\n    <meta charset=\"utf-8\">\n    <link rel=\"stylesheet\" href=\"a.css\"/>\n    <title>Doc</title>\n</head>\n"
    );
    // A slash on a void element is optional; `<br>` is inline, so its container stays.
    for doc in ["<p>a<br>b<br/>c</p>\n", "<div><img src=\"a.png\"></div>\n"] {
        assert_eq!(
            format_html(doc, FormatIndent::Two),
            FormatOutcome::AlreadyFormatted,
            "{doc:?}"
        );
    }
    assert_eq!(
        html("<div><hr><hr/></div>"),
        "<div>\n  <hr>\n  <hr/>\n</div>"
    );
}

#[test]
fn html_keeps_attributes_in_their_spelling_and_order() {
    let src = "<div>\n<section  class=wide   id=\"x\"  data-v='a  b' hidden>\n<p>t</p>\n</section>\n</div>\n";
    assert_eq!(
        html(src),
        "<div>\n  <section  class=wide   id=\"x\"  data-v='a  b' hidden>\n    <p>t</p>\n  </section>\n</div>\n"
    );
    // A `>` inside a quoted value does not end the tag; an unquoted value takes the slash.
    let quoted = "<div><p title=\"a > b\" data-x=a/b onclick='f(\"<p>\")'>x</p></div>";
    assert_eq!(
        html(quoted),
        "<div>\n  <p title=\"a > b\" data-x=a/b onclick='f(\"<p>\")'>x</p>\n</div>"
    );
    // Tag names are matched case-insensitively and keep their spelling.
    assert_eq!(
        html("<DIV>\n<P>x</P>\n</div>\n"),
        "<DIV>\n  <P>x</P>\n</div>\n"
    );
}

#[test]
fn html_keeps_crlf_the_byte_order_mark_and_the_final_newline_state() {
    assert_eq!(
        run(
            format_html,
            "<div>\r\n<p>a</p>\r\n</div>\r\n",
            FormatIndent::Tab
        ),
        "<div>\r\n\t<p>a</p>\r\n</div>\r\n"
    );
    assert_eq!(
        html("\u{feff}<div><p>a</p></div>"),
        "\u{feff}<div>\n  <p>a</p>\n</div>"
    );
    assert_eq!(html("<div><p>a</p></div>\n"), "<div>\n  <p>a</p>\n</div>\n");
}

#[test]
fn html_leaves_text_after_the_root_alone() {
    let src = "<div>\n<p>a</p>\n</div>\ntrailing text\n";
    assert_eq!(
        format_html(src, FormatIndent::Two),
        FormatOutcome::AlreadyFormatted
    );
}

#[test]
fn html_refuses_malformed_documents_and_names_the_offset() {
    let cases: &[(&str, RefusalReason, usize)] = &[
        ("<div><p>text</div>", RefusalReason::MismatchedTag, 12),
        ("<div>\n<p>x</p>\n", RefusalReason::UnclosedTag, 0),
        ("<ul><li>a<li>b</ul>", RefusalReason::MismatchedTag, 14),
        ("<p>a</p></div>", RefusalReason::MismatchedTag, 8),
        (
            "<div class=\"a>text</div>",
            RefusalReason::UnterminatedAttribute,
            11,
        ),
        ("<div><!-- x</div>", RefusalReason::UnterminatedComment, 5),
        ("<div><% x %></div>", RefusalReason::UnsupportedSyntax, 5),
        ("<script>var a;", RefusalReason::UnclosedTag, 0),
        // `</script` ends the script only when a delimiter follows it; the end tag itself may
        // run into the end of the document.
        ("<script>a</script", RefusalReason::UnclosedTag, 0),
        ("<script>a</script ", RefusalReason::UnexpectedEnd, 18),
        ("<div></div", RefusalReason::UnexpectedEnd, 10),
        // `<!--<script>` starts the tokenizer's double-escaped state, in which `</script>` does
        // not end the script.
        (
            "<script><!--<script></script>",
            RefusalReason::UnsupportedSyntax,
            0,
        ),
        ("<!x", RefusalReason::UnterminatedConstruct, 0),
        ("<!DOCTYPE html", RefusalReason::UnterminatedConstruct, 0),
        ("<div b=", RefusalReason::UnexpectedEnd, 7),
        ("<div", RefusalReason::UnexpectedEnd, 4),
        ("   \n", RefusalReason::EmptyDocument, 4),
    ];
    for &(src, reason, offset) in cases {
        assert_refused(format_html, src, reason, offset);
    }
    // `<div/>` opens a div in HTML, so it needs its end tag.
    assert_refused(format_html, "<div/>", RefusalReason::UnclosedTag, 0);
}

#[test]
fn html_formatting_twice_equals_formatting_once() {
    let docs = [
        "<!DOCTYPE html><html><head><title>T</title></head><body><div><p>a</p></div></body></html>\n",
        "<div>\n<pre>  a\n    b  </pre>\n<section><textarea>\n raw\n</textarea></section>\n</div>\n",
        "<head><SCRIPT>a</b>  b</SCRIPT ><style>  x  </STYLE></head>",
        "<body>\n<p>Hello <b>big</b>   world</p>\n<div>\n<span>a</span> <span>b</span>\n</div>\n</body>\n",
        "<div><!--><!--->\n<!-- a --!>\n<?php echo 1; ?>\n<p>x</p></div>",
        "<div>\r\n<p>a</p>\r\n\r\n\r\n<p>b</p>\r\n</div>\r\n",
        "<ul><li style=\"display:inline\">a</li><li style=\"display:inline\">b</li></ul>",
        "<div><ul><li style=\"display:inline-flex\">a</li><li>b</li></ul><p>x</p></div>",
        "<div>\n<section style=\"white-space: pre\">\n<p>a</p>\n</section>\n<p>b</p>\n</div>\n",
        "<div><![CDATA[x]]><p>a</p></div>",
        "<div><!foo bar><p>a</p></div>",
        "<div><!-- a -- b --! c --><p>x</p></div>",
        "<div>\r<p>a</p>\r</div>\r",
    ];
    for doc in docs {
        assert_stable(format_html, doc);
    }
}

const HTML_NODES: &[&str] = &[
    "<!DOCTYPE html>",
    "<html>",
    "<head>",
    "<meta charset=\"utf-8\">",
    "<title>T</title>",
    "<style>b { c: d }</style>",
    "</head>",
    "<body>",
    "<div>",
    "<p>a <b>b</b></p>",
    "<ul>",
    "<li>x</li>",
    "<li>y</li>",
    "</ul>",
    "</div>",
    "<div>",
    "<span>s</span>",
    "<span>t</span>",
    "</div>",
    "<pre>  p\n q</pre>",
    "<!-- c -->",
    "</body>",
    "</html>",
];

#[test]
fn html_with_randomly_woven_gaps_formats_stably() {
    for seed in 1..=300 {
        let src = weave(&mut Rng::new(seed), HTML_NODES, MARKUP_GAPS);
        assert_stable(format_html, &src);
    }
}
