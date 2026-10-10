//! HTML → Markdown paste conversion over real clipboard HTML (Word, Google Docs, Google Sheets,
//! browsers, VS Code) taken from third-party fixtures.

use std::path::PathBuf;

use ssw_convert::clipboard::{build_cf_html, parse_cf_html};
use ssw_convert::html_to_md::{HtmlPaste, convert_html_paste, fenced_code, html_to_markdown};
use ssw_convert::import::{Loss, LossKind};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> String {
    let path = root()
        .join("../../tests/fixtures/html")
        .join(format!("{name}.html"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn golden(name: &str, extension: &str) -> String {
    let path = root()
        .join("tests/golden/html")
        .join(format!("{name}.{extension}"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn pasted_markdown(name: &str) -> ssw_convert::import::ImportResult {
    match convert_html_paste(&fixture(name), false, "assets") {
        HtmlPaste::Markdown(result) => result,
        HtmlPaste::PlainText(text) => panic!("{name} pasted as plain text: {text:?}"),
    }
}

#[test]
fn golden_markdown() {
    for name in [
        "lexical-word",
        "lexical-gdocs-textformat",
        "lexical-gdocs-fontsize-pt",
        "lexical-gdocs-fontsize-px-pt",
        "lexical-gdocs-table",
        "lexical-gsheets-table",
        "lexical-browser-table",
    ] {
        let result = pasted_markdown(name);
        assert_eq!(result.markdown, golden(name, "md"), "{name}");
        assert!(result.assets.is_empty(), "{name}");
    }
}

#[test]
fn vscode_html_is_plain_text_with_or_without_the_vscode_format() {
    let expected = golden("lexical-vscode-safari", "txt");
    let html = fixture("lexical-vscode-safari");
    assert_eq!(
        convert_html_paste(&html, false, "assets"),
        HtmlPaste::PlainText(expected.clone())
    );
    assert_eq!(
        convert_html_paste(&html, true, "assets"),
        HtmlPaste::PlainText(expected)
    );
}

#[test]
fn rich_html_is_not_mistaken_for_code_even_when_the_vscode_format_is_absent() {
    for name in [
        "lexical-word",
        "lexical-gdocs-textformat",
        "lexical-gsheets-table",
    ] {
        assert!(
            matches!(
                convert_html_paste(&fixture(name), false, ""),
                HtmlPaste::Markdown(_)
            ),
            "{name}"
        );
    }
}

#[test]
fn losses_name_what_markdown_cannot_keep() {
    let word = pasted_markdown("lexical-word");
    assert_eq!(
        word.losses,
        [Loss {
            kind: LossKind::ListNumberingStyle,
            count: 2,
            detail: None
        }],
        "lettered sub-list markers cannot be kept"
    );
    for name in [
        "lexical-gdocs-table",
        "lexical-gsheets-table",
        "lexical-browser-table",
        "lexical-gdocs-textformat",
    ] {
        assert!(pasted_markdown(name).losses.is_empty(), "{name}");
    }
}

#[test]
fn clipboard_round_trip_keeps_the_conversion() {
    for name in ["lexical-gdocs-textformat", "lexical-gdocs-table"] {
        let html = fixture(name);
        let payload = build_cf_html(&html, Some("https://docs.google.com/document/d/x"));
        let parsed = parse_cf_html(payload.as_bytes()).unwrap();
        assert_eq!(parsed.fragment, html, "{name}");
        assert_eq!(
            parsed.source_url.as_deref(),
            Some("https://docs.google.com/document/d/x")
        );
        assert_eq!(
            html_to_markdown(&parsed.fragment, "assets").markdown,
            golden(name, "md"),
            "{name}"
        );
    }
}

#[test]
fn fenced_code_outgrows_backtick_runs() {
    let text = golden("lexical-vscode-safari", "txt");
    let fenced = fenced_code(&text);
    assert!(fenced.starts_with("```\n## 📱") && fenced.ends_with("improvements\n```"));
    assert_eq!(fenced_code("a ``` b"), "````\na ``` b\n````");
}

#[test]
fn a_word_list_paragraph_without_content_adds_no_item() {
    // Word pads empty list paragraphs with a no-break space inside `<o:p>`.
    let html = "<p class=MsoListParagraph style='mso-list:l0 level1 lfo1'>\
                <span style='mso-list:Ignore'>o</span>A</p>\
                <p style='mso-list:l0 level1 lfo1'><o:p>&nbsp;</o:p></p>";
    assert_eq!(html_to_markdown(html, "assets").markdown, "- A\n");
}
