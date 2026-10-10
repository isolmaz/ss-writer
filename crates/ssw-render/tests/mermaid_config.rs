//! `%%{init}%%` directives, front matter and error classification of `render_mermaid`.

use std::time::Duration;

use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, UnsupportedReason, render_mermaid_within};

const LIMIT: Duration = Duration::from_secs(120);

fn svg_of(src: &str, theme: MermaidTheme) -> String {
    match render_mermaid_within(src, theme, LIMIT) {
        MermaidOutcome::Diagram(_, svg) => svg,
        MermaidOutcome::Unsupported { reason } => panic!("unsupported: {reason}"),
    }
}

fn reason_of(src: &str) -> UnsupportedReason {
    match render_mermaid_within(src, MermaidTheme::Light, LIMIT) {
        MermaidOutcome::Unsupported { reason } => reason,
        MermaidOutcome::Diagram(..) => panic!("diagram unexpectedly rendered"),
    }
}

const FLOW: &str = "flowchart LR\n  A[Start] --> B[End]\n";

#[test]
fn init_directive_switches_theme() {
    let plain = svg_of(FLOW, MermaidTheme::Light);
    let dark = svg_of(
        &format!("%%{{init: {{\"theme\": \"dark\"}}}}%%\n{FLOW}"),
        MermaidTheme::Light,
    );
    assert!(!plain.contains("#333333"));
    assert!(dark.contains("#333333"));
}

#[test]
fn multiline_directive_and_theme_variables_apply() {
    let svg = svg_of(
        &format!(
            "%%{{\n  init: {{\n    \"themeVariables\": {{\"primaryColor\": \"#123456\"}}\n  }}\n}}%%\n{FLOW}"
        ),
        MermaidTheme::Light,
    );
    assert!(svg.contains("#123456"));
}

#[test]
fn front_matter_config_applies_and_directive_overrides_it() {
    let front = format!("---\nconfig:\n  theme: dark\n---\n{FLOW}");
    assert!(svg_of(&front, MermaidTheme::Light).contains("#333333"));

    let overridden = format!(
        "---\nconfig:\n  theme: dark\n---\n%%{{init: {{\"theme\": \"modern\"}}}}%%\n{FLOW}"
    );
    assert!(!svg_of(&overridden, MermaidTheme::Dark).contains("#333333"));
}

#[test]
fn font_is_segoe_ui_unless_configured() {
    assert!(svg_of(FLOW, MermaidTheme::Light).contains("Segoe UI"));
    let svg = svg_of(
        &format!("%%{{init: {{\"fontFamily\": \"Cascadia Mono\"}}}}%%\n{FLOW}"),
        MermaidTheme::Light,
    );
    assert!(svg.contains("Cascadia Mono"));
}

#[test]
fn unsupported_diagram_types_fall_back() {
    assert_eq!(
        reason_of("sankey-beta\n\nA,B,10\n"),
        UnsupportedReason::DiagramType("sankey-beta")
    );
    assert_eq!(
        reason_of("block-beta\n  columns 2\n  a b\n"),
        UnsupportedReason::DiagramType("block")
    );
}

#[test]
fn invalid_sources_are_syntax_errors() {
    assert!(matches!(
        reason_of("this is not mermaid"),
        UnsupportedReason::Syntax(_)
    ));
    assert!(matches!(reason_of(""), UnsupportedReason::Syntax(_)));
    assert!(matches!(
        reason_of("%%{init: nope}%%\nflowchart LR\nA-->B"),
        UnsupportedReason::Syntax(_)
    ));
}
