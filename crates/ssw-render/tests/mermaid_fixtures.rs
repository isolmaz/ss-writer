//! Every Mermaid file under `tests/fixtures/mermaid` (taken verbatim from the Mermaid docs, see
//! `tests/fixtures/SOURCES.md`) goes through the full pipeline in both palettes.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, UnsupportedReason, render_mermaid_within};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mermaid")
}

fn fixtures() -> Vec<(String, String)> {
    let mut files: Vec<(String, String)> = fs::read_dir(fixture_dir())
        .expect("fixture directory")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mmd"))
        .map(|path| {
            let name = path
                .file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned();
            (name, fs::read_to_string(&path).expect("utf-8 fixture"))
        })
        .collect();
    files.sort();
    files
}

/// Debug builds are slow; the 500 ms editor limit is covered by the unit tests of the guard.
const LIMIT: Duration = Duration::from_secs(120);

#[test]
fn every_fixture_is_classified() {
    let files = fixtures();
    assert!(files.len() >= 30, "fixtures missing: {}", files.len());
    for (name, source) in &files {
        for theme in [MermaidTheme::Light, MermaidTheme::Dark] {
            let outcome = render_mermaid_within(source, theme, LIMIT);
            let expected_fallback = if name.starts_with("sankey-") {
                Some("sankey-beta")
            } else if name.starts_with("block-") {
                Some("block")
            } else {
                None
            };
            match (expected_fallback, outcome) {
                (Some(kind), MermaidOutcome::Unsupported { reason }) => {
                    assert_eq!(reason, UnsupportedReason::DiagramType(kind), "{name}");
                }
                (None, MermaidOutcome::Diagram(scene, svg)) => {
                    assert!(svg.contains("<svg"), "{name}: no svg");
                    assert!(
                        scene.size.width > 20.0 && scene.size.height > 20.0,
                        "{name}"
                    );
                    let bounds = scene
                        .bounds()
                        .unwrap_or_else(|| panic!("{name}: empty scene"));
                    assert!(
                        bounds.width > 10.0 && bounds.height > 10.0,
                        "{name}: {bounds:?}"
                    );
                }
                (expected, outcome) => {
                    panic!("{name} ({theme:?}): expected {expected:?}, got {outcome:?}")
                }
            }
        }
    }
}

#[test]
fn gantt_fixture_axis_covers_its_three_weeks() {
    // Spike output: axis labels 2014-01-01 .. 2058-03-17 and every bar at zero width. The tasks
    // of `gantt-02.mmd` run from 2014-01-06 to the 2014-01-25 milestone.
    let source = fs::read_to_string(fixture_dir().join("gantt-02.mmd")).expect("gantt fixture");
    let MermaidOutcome::Diagram(_, svg) =
        render_mermaid_within(&source, MermaidTheme::Light, LIMIT)
    else {
        panic!("gantt fixture must render");
    };
    assert!(
        svg.contains(">2014-01-"),
        "tick labels are January 2014 dates"
    );
    assert!(
        !svg.contains("2014-02") && !svg.contains("2015-"),
        "axis ends at 2014-01-25"
    );
}

#[test]
fn dark_and_light_palettes_differ() {
    let source = fs::read_to_string(fixture_dir().join("pie-01.mmd")).expect("pie fixture");
    let light = render_mermaid_within(&source, MermaidTheme::Light, LIMIT);
    let dark = render_mermaid_within(&source, MermaidTheme::Dark, LIMIT);
    let (MermaidOutcome::Diagram(light, light_svg), MermaidOutcome::Diagram(dark, dark_svg)) =
        (light, dark)
    else {
        panic!("pie fixture must render");
    };
    assert_ne!(light_svg, dark_svg);
    assert_ne!(light.items, dark.items);
    assert!(dark_svg.contains("#333333"));
    assert!(!light_svg.contains("#333333"));
}
