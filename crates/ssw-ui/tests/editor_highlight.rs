//! Syntax highlighting: token tables per language, incremental caching, language names and
//! theme colors.

use ssw_ui::editor::highlight::{Highlighter, Language, SyntaxColors, Token, TokenKind, tokenize};
use ssw_ui::geometry::Color;
use ssw_ui::text::{FontWeight, TextStyle};
use ssw_ui::theme::{DEFAULT_ACCENT, SystemColors, Theme};

use TokenKind::{
    Added, Attribute, Comment, Constant, Escape, Function, Heading, Keyword, Number, Property,
    Removed, String as Str, Tag, Type, Variable,
};

fn toks(language: Language, text: &str) -> Vec<(&str, TokenKind)> {
    tokenize(language, text)
        .into_iter()
        .map(|token| (&text[token.start..token.end], token.kind))
        .collect()
}

#[test]
fn rust_tokens() {
    assert_eq!(
        toks(Language::Rust, "let x: u32 = 42; // note\n"),
        [
            ("let", Keyword),
            ("u32", Type),
            ("42", Number),
            ("// note", Comment)
        ]
    );
    assert_eq!(
        toks(Language::Rust, r#""a\n""#),
        [("\"a", Str), ("\\n", Escape), ("\"", Str)]
    );
    assert_eq!(
        toks(Language::Rust, "'a' 'static"),
        [("'a'", Str), ("'static", Type)]
    );
    assert_eq!(
        toks(Language::Rust, r#"println!("x")"#),
        [("println", Function), ("\"x\"", Str)]
    );
    assert_eq!(
        toks(Language::Rust, "#[derive(Debug)] struct Foo;"),
        [
            ("#[derive(Debug)]", Attribute),
            ("struct", Keyword),
            ("Foo", Type)
        ]
    );
    assert_eq!(
        toks(Language::Rust, "/* a /* b */ c */ x"),
        [("/* a /* b */ c */", Comment)]
    );
    assert_eq!(
        toks(Language::Rust, r##"r#"a"b"#"##),
        [(r##"r#"a"b"#"##, Str)]
    );
}

#[test]
fn block_comments_continue_on_the_next_line() {
    let text = "/* a\n b */ x";
    assert_eq!(
        toks(Language::C, text),
        [("/* a", Comment), (" b */", Comment)]
    );
}

#[test]
fn c_and_cpp_tokens() {
    assert_eq!(
        toks(Language::C, "#include <stdio.h>"),
        [("#include", Keyword), ("<stdio.h>", Str)]
    );
    assert_eq!(
        toks(Language::C, "int main(void) { return 0; }"),
        [
            ("int", Type),
            ("main", Function),
            ("void", Type),
            ("return", Keyword),
            ("0", Number)
        ]
    );
    assert_eq!(
        toks(Language::Cpp, "std::string s = nullptr;"),
        [("string", Type), ("nullptr", Constant)]
    );
}

#[test]
fn csharp_and_java_tokens() {
    assert_eq!(
        toks(Language::CSharp, r#"var s = $"hi {x}";"#),
        [("var", Keyword), ("$\"hi {x}\"", Str)]
    );
    assert_eq!(
        toks(Language::CSharp, r#"@"a""b""#),
        [("@\"a", Str), ("\"\"", Escape), ("b\"", Str)]
    );
    assert_eq!(
        toks(Language::Java, "@Override public void run() {}"),
        [
            ("@Override", Attribute),
            ("public", Keyword),
            ("void", Type),
            ("run", Function)
        ]
    );
}

#[test]
fn javascript_and_typescript_tokens() {
    assert_eq!(
        toks(Language::JavaScript, "const s = `a${b}c`;"),
        [
            ("const", Keyword),
            ("`a", Str),
            ("${", Escape),
            ("}", Escape),
            ("c`", Str)
        ]
    );
    assert_eq!(
        toks(Language::TypeScript, "interface Foo { x: string }"),
        [("interface", Keyword), ("Foo", Type), ("string", Type)]
    );
}

#[test]
fn json_keys_are_properties() {
    assert_eq!(
        toks(Language::Json, r#"{"a": [1, true, null], "b": "x\n"}"#),
        [
            ("\"a\"", Property),
            ("1", Number),
            ("true", Constant),
            ("null", Constant),
            ("\"b\"", Property),
            ("\"x", Str),
            ("\\n", Escape),
            ("\"", Str)
        ]
    );
}

#[test]
fn python_and_go_tokens() {
    assert_eq!(
        toks(Language::Python, "def go(n=0x1F):"),
        [("def", Keyword), ("go", Function), ("0x1F", Number)]
    );
    assert_eq!(
        toks(Language::Python, "print(\"hi\")  # c"),
        [("print", Function), ("\"hi\"", Str), ("# c", Comment)]
    );
    assert_eq!(
        toks(Language::Python, "\"\"\"a\nb\"\"\""),
        [("\"\"\"a", Str), ("b\"\"\"", Str)]
    );
    assert_eq!(toks(Language::Python, r#"rb"\d""#), [(r#"rb"\d""#, Str)]);
    assert_eq!(
        toks(Language::Go, "package main\nx := nil"),
        [("package", Keyword), ("nil", Constant)]
    );
}

#[test]
fn shell_tokens() {
    assert_eq!(
        toks(Language::Bash, "echo \"$HOME\" -n # c"),
        [
            ("echo", Function),
            ("\"", Str),
            ("$HOME", Variable),
            ("\"", Str),
            ("-n", Attribute),
            ("# c", Comment)
        ]
    );
    assert_eq!(
        toks(Language::PowerShell, "Get-ChildItem -Path $env:TEMP # c"),
        [
            ("Get-ChildItem", Function),
            ("-Path", Attribute),
            ("$env:TEMP", Variable),
            ("# c", Comment)
        ]
    );
}

#[test]
fn sql_tokens_ignore_case() {
    assert_eq!(
        toks(
            Language::Sql,
            "select count(*) from t where a = 'it''s' -- c"
        ),
        [
            ("select", Keyword),
            ("count", Function),
            ("from", Keyword),
            ("where", Keyword),
            ("'it", Str),
            ("''", Escape),
            ("s'", Str),
            ("-- c", Comment)
        ]
    );
}

#[test]
fn markup_tokens() {
    assert_eq!(
        toks(Language::Html, "<a href=\"x\">t &amp; u</a><!-- c -->"),
        [
            ("<a", Tag),
            ("href", Attribute),
            ("\"x\"", Str),
            ("&amp;", Escape),
            ("</a", Tag),
            ("<!-- c -->", Comment)
        ]
    );
    assert_eq!(
        toks(Language::Xml, "<?xml version=\"1.0\"?>"),
        [("<?xml version=\"1.0\"?>", Keyword)]
    );
    assert_eq!(
        toks(
            Language::Css,
            "a:hover { color: #fff; margin: 10px; } /* c */"
        ),
        [
            ("a", Tag),
            ("hover", Attribute),
            ("color", Property),
            ("#fff", Number),
            ("margin", Property),
            ("10px", Number),
            ("/* c */", Comment)
        ]
    );
}

#[test]
fn data_language_tokens() {
    assert_eq!(
        toks(Language::Yaml, "key: 'v' # c\n- name: true\n"),
        [
            ("key", Property),
            ("'v'", Str),
            ("# c", Comment),
            ("name", Property),
            ("true", Constant)
        ]
    );
    assert_eq!(
        toks(Language::Toml, "[server]\nport = 8080 # p\nname = \"x\"\n"),
        [
            ("[server]", Type),
            ("port", Property),
            ("8080", Number),
            ("# p", Comment),
            ("name", Property),
            ("\"x\"", Str)
        ]
    );
}

#[test]
fn markdown_and_diff_tokens() {
    assert_eq!(
        toks(
            Language::Markdown,
            "# Title\n- item `code` [a](b)\n```rs\nlet x\n```\n"
        ),
        [
            ("# Title", Heading),
            ("- ", Keyword),
            ("`code`", Str),
            ("[a]", Function),
            ("(b)", Constant),
            ("```", Comment),
            ("rs", Type),
            ("let x", Str),
            ("```", Comment)
        ]
    );
    assert_eq!(
        toks(
            Language::Diff,
            "--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n ctx\n"
        ),
        [
            ("--- a", Keyword),
            ("+++ b", Keyword),
            ("@@ -1 +1 @@", Type),
            ("-old", Removed),
            ("+new", Added)
        ]
    );
}

#[test]
fn tokens_are_ordered_disjoint_and_on_char_boundaries() {
    let sample = "fn é() { let s = \"ü\\n\"; /* ö */ } // ñ\n<p a='é'>&amp;</p>\n$x = 'ß' # ü\n";
    for language in Language::ALL {
        let tokens = tokenize(language, sample);
        let mut end = 0;
        for token in tokens {
            assert!(
                token.start >= end && token.start < token.end,
                "{language:?}"
            );
            assert!(sample.is_char_boundary(token.start) && sample.is_char_boundary(token.end));
            end = token.end;
        }
    }
}

#[test]
fn highlighter_rescans_only_changed_lines() {
    let mut highlighter = Highlighter::new();
    let first = highlighter.highlight(7, Language::Rust, "let a = 1;\nlet b = 2;\nlet c = 3;\n");
    assert_eq!(highlighter.last_scanned_lines(), 3);
    let again = highlighter.highlight(7, Language::Rust, "let a = 1;\nlet b = 2;\nlet c = 3;\n");
    assert_eq!(highlighter.last_scanned_lines(), 0);
    assert_eq!(first, again);
    let edited = highlighter.highlight(7, Language::Rust, "let a = 1;\nlet b = 22;\nlet c = 3;\n");
    assert_eq!(highlighter.last_scanned_lines(), 1);
    assert_eq!(
        edited,
        tokenize(Language::Rust, "let a = 1;\nlet b = 22;\nlet c = 3;\n")
    );
}

#[test]
fn closing_a_comment_rescans_the_lines_it_covered() {
    let mut highlighter = Highlighter::new();
    highlighter.highlight(1, Language::C, "x\n/* c\ny\nz\n");
    assert_eq!(highlighter.last_scanned_lines(), 4);
    let tokens = highlighter.highlight(1, Language::C, "x\n/* c */\ny\nz\n");
    assert_eq!(highlighter.last_scanned_lines(), 3);
    assert_eq!(tokens, tokenize(Language::C, "x\n/* c */\ny\nz\n"));
    highlighter.highlight(1, Language::Python, "x\n");
    assert_eq!(highlighter.last_scanned_lines(), 1);
}

#[test]
fn the_cache_is_bounded() {
    let mut highlighter = Highlighter::new();
    for key in 0..300 {
        highlighter.highlight(key, Language::C, "int x;\n");
    }
    assert_eq!(highlighter.cached_blocks(), 256);
}

#[test]
fn language_names_and_aliases() {
    for language in Language::ALL {
        assert_eq!(Language::from_name(language.name()), Some(language));
    }
    for (name, language) in [
        ("rs", Language::Rust),
        ("C++", Language::Cpp),
        ("c#", Language::CSharp),
        ("JS", Language::JavaScript),
        ("tsx", Language::TypeScript),
        ("py", Language::Python),
        ("sh", Language::Bash),
        ("ps1", Language::PowerShell),
        ("yml", Language::Yaml),
        ("md", Language::Markdown),
        ("patch", Language::Diff),
        ("{.rust}", Language::Rust),
        ("rust,ignore", Language::Rust),
    ] {
        assert_eq!(Language::from_name(name), Some(language), "{name}");
    }
    assert_eq!(Language::from_name("brainfuck"), None);
    assert_eq!(Language::from_name(""), None);
}

#[test]
fn syntax_colors_come_from_the_theme() {
    let theme = Theme::light(DEFAULT_ACCENT);
    let colors = SyntaxColors::from_theme(&theme);
    let tokens = &theme.tokens;
    assert_eq!(colors.color(TokenKind::Plain), tokens.text);
    assert_eq!(colors.color(Comment), tokens.text_muted);
    assert_eq!(colors.color(Keyword), tokens.callout.important.bar);
    assert_eq!(colors.color(Str), tokens.callout.tip.bar);
    assert_eq!(colors.color(Removed), tokens.error);
    let dark = Theme::dark(DEFAULT_ACCENT);
    assert_ne!(
        SyntaxColors::from_theme(&dark).color(Keyword),
        colors.color(Keyword)
    );
}

/// What `toks` returns for text without any token.
const NO_TOKENS: [(&str, TokenKind); 0] = [];

const KINDS: [TokenKind; 16] = [
    TokenKind::Plain,
    Keyword,
    Type,
    Constant,
    Function,
    Number,
    Str,
    Escape,
    Comment,
    Property,
    Tag,
    Attribute,
    Variable,
    Added,
    Removed,
    Heading,
];

#[test]
fn every_token_kind_has_its_theme_color() {
    let theme = Theme::light(DEFAULT_ACCENT);
    let colors = SyntaxColors::from_theme(&theme);
    let tokens = &theme.tokens;
    let callout = &tokens.callout;
    let expected = [
        tokens.text,
        callout.important.bar,
        callout.note.bar,
        callout.warning.bar,
        tokens.accent,
        callout.warning.bar,
        callout.tip.bar,
        callout.caution.bar,
        tokens.text_muted,
        callout.note.bar,
        callout.important.bar,
        callout.warning.bar,
        tokens.accent,
        tokens.success,
        tokens.error,
        tokens.accent,
    ];
    for (kind, color) in KINDS.into_iter().zip(expected) {
        assert_eq!(colors.color(kind), color, "{kind:?}");
    }
}

#[test]
fn high_contrast_colors_everything_but_comments_like_the_text() {
    let system = SystemColors {
        window: Color::BLACK,
        window_text: Color::WHITE,
        gray_text: Color::from_rgb_u32(0x3FF23F),
        highlight: Color::from_rgb_u32(0x1AEBFF),
        highlight_text: Color::BLACK,
        button_face: Color::from_rgb_u32(0x202020),
        button_text: Color::WHITE,
        hotlight: Color::from_rgb_u32(0xFFFF00),
    };
    let theme = Theme::high_contrast(system);
    let colors = SyntaxColors::from_theme(&theme);
    assert_ne!(theme.tokens.text, theme.tokens.text_muted);
    for kind in KINDS {
        let wanted = if kind == Comment {
            theme.tokens.text_muted
        } else {
            theme.tokens.text
        };
        assert_eq!(colors.color(kind), wanted, "{kind:?}");
    }
}

#[test]
fn styled_ranges_shift_into_the_display_text_and_style_comments_and_headings() {
    let theme = Theme::light(DEFAULT_ACCENT);
    let colors = SyntaxColors::from_theme(&theme);
    let base = TextStyle::new("Mono", 12.0);
    let token = |kind| Token {
        start: 2,
        end: 5,
        kind,
    };

    let comment = colors.styled(&token(Comment), &base, 10);
    assert_eq!(comment.range, 12..15);
    assert!(comment.style.italic);
    assert_eq!(comment.style.weight, base.weight);
    assert_eq!(comment.style.color, Some(colors.color(Comment)));

    let heading = colors.styled(&token(Heading), &base, 0);
    assert_eq!(heading.range, 2..5);
    assert_eq!(heading.style.weight, FontWeight::BOLD);
    assert!(!heading.style.italic);
    assert_eq!(heading.style.color, Some(colors.color(Heading)));

    let keyword = colors.styled(&token(Keyword), &base, 3);
    assert_eq!(keyword.range, 5..8);
    assert!(!keyword.style.italic);
    assert_eq!(keyword.style.weight, base.weight);
    assert_eq!(keyword.style.color, Some(colors.color(Keyword)));
}

#[test]
fn escape_sequences_are_their_own_tokens() {
    assert_eq!(
        toks(Language::Rust, r#""\x41\u{1F600}\u00e9\U0001F600\012\q""#),
        [
            ("\"", Str),
            ("\\x41", Escape),
            ("\\u{1F600}", Escape),
            ("\\u00e9", Escape),
            ("\\U0001F600", Escape),
            ("\\012", Escape),
            ("\\q", Escape),
            ("\"", Str)
        ]
    );
    // A backslash at the end of the line escapes nothing but still stands apart.
    assert_eq!(
        toks(Language::Rust, r#""ab\"#),
        [("\"ab", Str), ("\\", Escape)]
    );
}

#[test]
fn rust_quotes_are_characters_lifetimes_or_plain() {
    assert_eq!(
        toks(Language::Rust, r"'\n'"),
        [("'", Str), ("\\n", Escape), ("'", Str)]
    );
    assert_eq!(toks(Language::Rust, "b'x'"), [("b", Str), ("'x'", Str)]);
    assert_eq!(toks(Language::Rust, "'\\n"), NO_TOKENS);
    assert_eq!(toks(Language::Rust, "x = '"), NO_TOKENS);
    assert_eq!(toks(Language::Rust, "'1 "), [("1", Number)]);
    assert_eq!(toks(Language::Rust, "r#match"), [("match", Keyword)]);
    assert_eq!(
        toks(Language::Rust, "#[a[1]]"),
        [("#[a[1]]", Attribute)],
        "brackets nest inside an attribute"
    );
}

#[test]
fn numbers_have_fractions_exponents_and_suffixes() {
    assert_eq!(
        toks(Language::Rust, "3.14 1e10 1e+5 2.5E-3 42u32 0xFFu8"),
        [
            ("3.14", Number),
            ("1e10", Number),
            ("1e+5", Number),
            ("2.5E-3", Number),
            ("42u32", Number),
            ("0xFFu8", Number)
        ]
    );
}

#[test]
fn all_caps_words_are_constants_in_languages_that_say_so() {
    assert_eq!(
        toks(Language::Rust, "MAX_SIZE + A + Ab"),
        [("MAX_SIZE", Constant), ("Ab", Type)]
    );
}

#[test]
fn raw_strings_continue_over_lines() {
    assert_eq!(
        toks(Language::Rust, "r#\"a\nb\nc\"# d"),
        [("r#\"a", Str), ("b", Str), ("c\"#", Str)]
    );
}

#[test]
fn unterminated_template_interpolation_stays_in_the_string() {
    assert_eq!(toks(Language::JavaScript, "`a ${b"), [("`a ${b", Str)]);
}

#[test]
fn shell_variables_inside_strings_and_lone_dollars() {
    assert_eq!(
        toks(Language::Bash, r#"echo "${HOME} $1 $? cost $ 5""#),
        [
            ("echo", Function),
            ("\"", Str),
            ("${HOME}", Variable),
            (" ", Str),
            ("$1", Variable),
            (" ", Str),
            ("$?", Variable),
            (" cost $ 5\"", Str)
        ]
    );
    assert_eq!(toks(Language::Bash, "\"cost $"), [("\"cost $", Str)]);
    assert_eq!(toks(Language::Bash, "$ x"), NO_TOKENS);
    assert_eq!(
        toks(Language::PowerShell, "$True $x"),
        [("$True", Constant), ("$x", Variable)],
        "PowerShell constants ignore case"
    );
}

#[test]
fn preprocessor_directives_without_angle_includes() {
    assert_eq!(
        toks(Language::C, "#define X 1\n#include \"x.h\"\n"),
        [
            ("#define", Keyword),
            ("1", Number),
            ("#include", Keyword),
            ("\"x.h\"", Str)
        ]
    );
}

#[test]
fn yaml_documents_and_keys() {
    assert_eq!(
        toks(Language::Yaml, "---\na: 1\n...\n"),
        [
            ("---", Keyword),
            ("a", Property),
            ("1", Number),
            ("...", Keyword)
        ]
    );
    assert_eq!(
        toks(Language::Yaml, "\"a b\": 1\n'k': v\n{a: 1}\n"),
        [
            ("\"a b\"", Property),
            ("1", Number),
            ("'k'", Property),
            ("1", Number)
        ],
        "quoted keys count, flow mappings have none"
    );
}

#[test]
fn toml_quoted_and_dotted_keys() {
    assert_eq!(
        toks(Language::Toml, "\"a b\" = 1\na.b = 2\n"),
        [
            ("\"a b\"", Property),
            ("1", Number),
            ("a.b", Property),
            ("2", Number)
        ]
    );
}

#[test]
fn html_comments_and_quoted_values_continue_across_lines() {
    assert_eq!(
        toks(Language::Html, "<!-- a\nb\nc --><p>x</p>"),
        [
            ("<!-- a", Comment),
            ("b", Comment),
            ("c -->", Comment),
            ("<p", Tag),
            ("</p", Tag)
        ]
    );
    assert_eq!(
        toks(Language::Html, "<a title=\"x\nmid\ny\" href='z'>"),
        [
            ("<a", Tag),
            ("title", Attribute),
            ("\"x", Str),
            ("mid", Str),
            ("y\"", Str),
            ("href", Attribute),
            ("'z'", Str)
        ]
    );
}

#[test]
fn html_self_closing_tags_stray_brackets_and_ampersands() {
    assert_eq!(
        toks(Language::Html, "<br/><b>"),
        [("<br", Tag), ("<b", Tag)]
    );
    assert_eq!(
        toks(Language::Xml, "a < b <i> & &amp; &x"),
        [("<i", Tag), ("&amp;", Escape)]
    );
}

#[test]
fn css_comments_at_rules_important_and_selectors() {
    assert_eq!(
        toks(Language::Css, "/* a\nb\nc */ x {}"),
        [
            ("/* a", Comment),
            ("b", Comment),
            ("c */", Comment),
            ("x", Tag)
        ]
    );
    assert_eq!(
        toks(
            Language::Css,
            ".a #b { color: #f00 !important; }\n@media print { }\n"
        ),
        [
            (".a", Type),
            ("#b", Type),
            ("color", Property),
            ("#f00", Number),
            ("!important", Keyword),
            ("@media", Keyword),
            ("print", Tag)
        ]
    );
}

#[test]
fn markdown_quotes_rules_numbered_lists_and_unclosed_spans() {
    assert_eq!(
        toks(Language::Markdown, "> quote\n1. one\n2) two\n---\n"),
        [
            (">", Comment),
            ("1. ", Keyword),
            ("2) ", Keyword),
            ("---", Keyword)
        ]
    );
    assert_eq!(
        toks(Language::Markdown, "**bold** and __b2__ **x\nx **** y\n"),
        [("**bold**", Variable), ("__b2__", Variable)]
    );
    assert_eq!(
        toks(Language::Markdown, "`x` and ` y\n[a](b\n[c\n"),
        [("`x`", Str)]
    );
}

#[test]
fn diff_headers_are_comments() {
    assert_eq!(
        toks(
            Language::Diff,
            "diff --git a/x b/x\nindex 1..2 100644\n+new\n"
        ),
        [
            ("diff --git a/x b/x", Comment),
            ("index 1..2 100644", Comment),
            ("+new", Added)
        ]
    );
}

#[test]
fn shell_options_need_a_letter_or_a_second_dash_after_the_dash() {
    assert_eq!(
        toks(Language::Bash, "echo --all - x"),
        [("echo", Function), ("--all", Attribute)],
        "a long option is one token, a lone dash is not an option"
    );
}

#[test]
fn an_exponent_marker_without_digits_ends_the_number_at_the_suffix() {
    assert_eq!(
        toks(Language::Rust, "1em 1e"),
        [("1em", Number), ("1e", Number)]
    );
}

#[test]
fn rust_raw_string_prefixes_are_plain_words_without_a_quote_or_hash() {
    assert_eq!(toks(Language::Rust, "r br cr"), NO_TOKENS);
}

#[test]
fn yaml_keys_must_be_complete_quoted_scalars_followed_by_a_colon() {
    assert_eq!(
        toks(Language::Yaml, "- "),
        NO_TOKENS,
        "a bare list dash has no key"
    );
    assert_eq!(
        toks(Language::Yaml, "\"abc: 1"),
        [("\"abc: 1", Str)],
        "an unterminated quoted key is an unterminated string"
    );
    assert_eq!(
        toks(Language::Yaml, "\"a\" b: c"),
        [("\"a\"", Str)],
        "a quoted scalar not followed by a colon is a value"
    );
}

#[test]
fn toml_quoted_keys_must_be_terminated() {
    assert_eq!(
        toks(Language::Toml, "\"abc = 1"),
        [("\"abc = 1", Str)],
        "an unterminated double-quoted key is an unterminated string"
    );
    assert_eq!(
        toks(Language::Toml, "'abc = 1"),
        [("'abc = 1", Str)],
        "an unterminated single-quoted key is an unterminated string"
    );
}

#[test]
fn markdown_fences_without_a_language_and_blank_lines_inside_them() {
    assert_eq!(
        toks(Language::Markdown, "```\n\ncode\n```\n"),
        [("```", Comment), ("code", Str), ("```", Comment)],
        "no language label token, no token for the blank line inside the fence"
    );
}

#[test]
fn markdown_only_dash_star_and_underscore_runs_are_rules_and_a_number_alone_is_no_list() {
    assert_eq!(
        toks(Language::Markdown, "***\n___\n"),
        [("***", Keyword), ("___", Keyword)]
    );
    assert_eq!(
        toks(Language::Markdown, "aaa\n===\n12\n1\n"),
        NO_TOKENS,
        "runs of other characters and digits without a marker are plain text"
    );
}
