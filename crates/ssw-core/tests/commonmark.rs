//! CommonMark 0.31.2 and GFM extension conformance: every example is parsed with
//! `parse_document`, rendered to HTML by `support/html.rs` and compared with the expected
//! HTML. Only the examples listed in `data/expected-failures.txt` (id + reason) may differ; an
//! example that is listed but passes is reported too, so the list never goes stale.

#[path = "support/html.rs"]
mod html;

use std::collections::BTreeMap;

use serde::Deserialize;
use ssw_core::markdown::parse_document;

const COMMONMARK: &str = include_str!("data/commonmark-0.31.2.json");
const GFM: &str = include_str!("data/gfm-extensions.txt");
const EXPECTED_FAILURES: &str = include_str!("data/expected-failures.txt");

#[derive(Deserialize)]
struct Example {
    markdown: String,
    html: String,
    example: u32,
}

struct Case {
    id: String,
    markdown: String,
    html: String,
    tagfilter: bool,
}

fn commonmark_cases() -> Vec<Case> {
    let examples: Vec<Example> = serde_json::from_str(COMMONMARK).expect("valid CommonMark JSON");
    examples
        .into_iter()
        .map(|e| Case {
            id: e.example.to_string(),
            markdown: e.markdown,
            html: e.html,
            tagfilter: false,
        })
        .collect()
}

/// Examples of the spec-test text format: fenced with 32 backticks + ` example`, markdown and
/// html separated by a line holding a single `.`; `→` stands for a tab.
fn gfm_cases() -> Vec<Case> {
    let mut cases = Vec::new();
    let mut lines = GFM.lines();
    let mut section = String::new();
    let mut n = 0;
    while let Some(line) = lines.next() {
        if let Some(title) = line.strip_prefix("## ") {
            section = title.to_string();
            continue;
        }
        if !line.starts_with("```````````````````````````````` example") {
            continue;
        }
        let mut markdown = String::new();
        let mut html = String::new();
        let mut in_html = false;
        for body in lines.by_ref() {
            if body == "````````````````````````````````" {
                break;
            }
            if body == "." && !in_html {
                in_html = true;
                continue;
            }
            let target = if in_html { &mut html } else { &mut markdown };
            target.push_str(&body.replace('→', "\t"));
            target.push('\n');
        }
        n += 1;
        cases.push(Case {
            id: format!("gfm-{n}"),
            markdown,
            html,
            tagfilter: section == "HTML tag filter",
        });
    }
    cases
}

fn expected_failures() -> BTreeMap<String, String> {
    EXPECTED_FAILURES
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once(char::is_whitespace) {
            Some((id, reason)) => (id.to_string(), reason.trim().to_string()),
            None => (l.to_string(), String::new()),
        })
        .collect()
}

#[test]
fn conformance() {
    let expected = expected_failures();
    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    let mut total = 0;
    for case in commonmark_cases().into_iter().chain(gfm_cases()) {
        total += 1;
        let doc = parse_document(&case.markdown);
        let actual = html::render(
            &doc,
            &case.markdown,
            &html::Options {
                tagfilter: case.tagfilter,
            },
        );
        // `<IGNORE>` marks examples that only check that parsing does not crash.
        let passed = case.html.trim() == "<IGNORE>" || actual == case.html;
        match (passed, expected.contains_key(&case.id)) {
            (false, false) => unexpected.push(format!(
                "example {}:\n--- markdown\n{:?}\n--- expected\n{:?}\n--- actual\n{:?}\n",
                case.id, case.markdown, case.html, actual
            )),
            (true, true) => stale.push(case.id.clone()),
            _ => {}
        }
    }
    assert!(total > 650, "example suites not loaded: {total}");
    assert!(
        unexpected.is_empty() && stale.is_empty(),
        "{} unexpected failures, {} expected failures that now pass: {:?}\n{}",
        unexpected.len(),
        stale.len(),
        stale,
        unexpected.join("\n")
    );
}

#[test]
fn expected_failures_have_reasons() {
    for (id, reason) in expected_failures() {
        assert!(!reason.is_empty(), "expected failure {id} needs a reason");
    }
}
