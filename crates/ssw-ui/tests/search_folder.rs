//! The "Search in folder" panel: requests, grouped results with previews and highlights,
//! collapsing, opening a match, keyboard flow, invalid regex, status line, access tree.

mod search_common;

use search_common::{Host, child_names, clipboard, covered, localizer, node_named};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::Rect;
use ssw_ui::input::{Modifiers, vk};
use ssw_ui::shell::folder_search::{
    FolderSearchEvent, FolderSearchFile, FolderSearchHit, FolderSearchLine, FolderSearchPanel,
    FolderSearchRequest, FolderSearchSummary,
};
use ssw_ui::widget::WidgetId;
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

const BOUNDS: Rect = Rect::new(0.0, 0.0, 300.0, 500.0);

fn setup() -> (Host, FolderSearchPanel) {
    let mut host = Host::new();
    let mut panel =
        FolderSearchPanel::new(localizer(), &clipboard()).with_motion(Motion::reduced());
    panel.open();
    host.layout(&mut panel, BOUNDS);
    let focus = panel.take_focus_request().expect("opening asks for focus");
    host.focus_on(focus);
    (host, panel)
}

fn line(number: u64, preview: &str, ranges: &[std::ops::Range<usize>]) -> FolderSearchLine {
    FolderSearchLine {
        line: number,
        column: ranges.first().map_or(0, |range| range.start),
        ranges: ranges.to_vec(),
        preview: preview.to_owned(),
    }
}

fn sample() -> Vec<FolderSearchFile> {
    vec![
        FolderSearchFile {
            path: "a.md".to_owned(),
            matches: vec![
                line(3, "  hello world", &[2..7]),
                line(10, "say hello, hello", &[4..9, 11..16]),
            ],
        },
        FolderSearchFile {
            path: "docs/b.md".to_owned(),
            matches: vec![line(1, "hello", &[0..5])],
        },
    ]
}

fn show_results(host: &mut Host, panel: &mut FolderSearchPanel) {
    panel.set_results(sample(), FolderSearchSummary::default());
    host.layout(panel, BOUNDS);
}

#[test]
fn enter_requests_a_search_and_the_toggles_repeat_it() {
    let (mut host, mut panel) = setup();
    host.text(&mut panel, "hello");
    assert!(host.key(&mut panel, vk::RETURN).handled);
    assert_eq!(
        panel.take_events(),
        vec![FolderSearchEvent::Search(FolderSearchRequest {
            pattern: "hello".to_owned(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
        })]
    );
    assert_eq!(panel.status_text(), "Searching…");
    // A toggle searches again with the new options.
    let tree = host.tree(&panel);
    let at = node_named(&tree, "Match case").bounds.center();
    host.click(&mut panel, at);
    let events = panel.take_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        &events[0],
        FolderSearchEvent::Search(request) if request.case_sensitive && request.pattern == "hello"
    ));
    panel.set_options(true, true, false);
    assert_eq!(
        panel.request(),
        FolderSearchRequest {
            pattern: "hello".to_owned(),
            case_sensitive: true,
            whole_word: true,
            regex: false,
        }
    );
}

#[test]
fn an_empty_query_clears_instead_of_searching() {
    let (mut host, mut panel) = setup();
    show_results(&mut host, &mut panel);
    assert!(!panel.list().is_empty());
    assert!(host.key(&mut panel, vk::RETURN).handled);
    assert_eq!(panel.take_events(), vec![FolderSearchEvent::Cleared]);
    assert!(panel.list().is_empty());
    assert!(panel.files().is_empty());
    assert_eq!(panel.status_text(), "");
    // A toggle without a query does nothing.
    let tree = host.tree(&panel);
    let at = node_named(&tree, "Regular expression").bounds.center();
    host.click(&mut panel, at);
    assert!(panel.take_events().is_empty());
}

#[test]
fn results_are_grouped_by_file_with_trimmed_previews_and_highlights() {
    let (mut host, mut panel) = setup();
    show_results(&mut host, &mut panel);
    let rows = panel.list().rows();
    assert_eq!(rows.len(), 5);
    assert!(rows[0].header && rows[0].expanded == Some(true));
    assert_eq!(
        (rows[0].label.as_str(), rows[0].detail.as_str()),
        ("a.md", "2")
    );
    assert_eq!(
        (rows[1].prefix.as_str(), rows[1].label.as_str()),
        ("3", "hello world")
    );
    assert_eq!(rows[1].highlights, vec![0..5]);
    assert_eq!(covered(&rows[2].label, &rows[2].highlights), "hellohello");
    assert_eq!(
        (rows[2].prefix.as_str(), rows[2].detail.as_str()),
        ("10", "")
    );
    assert!(rows[3].header);
    assert_eq!(rows[3].label, "docs/b.md");
    assert_eq!(rows[4].label, "hello");
    assert_eq!(panel.status_text(), "3 results in 2 files");
    // Painted: file names, previews and a pill behind every match.
    let accent = host.theme.tokens.accent.with_alpha(0.3);
    let canvas = host.paint(&panel);
    let texts = canvas.texts();
    assert!(
        texts.contains(&"a.md") && texts.contains(&"hello world") && texts.contains(&"docs/b.md")
    );
    assert!(texts.contains(&"3 results in 2 files"));
    let pills = canvas
        .ops()
        .filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == accent))
        .count();
    assert_eq!(pills, 4);
}

#[test]
fn the_status_line_reports_empty_truncated_and_invalid_searches() {
    let (mut host, mut panel) = setup();
    panel.set_results(Vec::new(), FolderSearchSummary::default());
    assert_eq!(panel.status_text(), "No results found");
    panel.set_results(sample(), FolderSearchSummary { truncated: true });
    assert_eq!(panel.status_text(), "Only the first 3 results are shown");
    // A single match reads in the singular.
    panel.set_results(
        vec![FolderSearchFile {
            path: "one.md".to_owned(),
            matches: vec![line(1, "x", &[0..1])],
        }],
        FolderSearchSummary::default(),
    );
    assert_eq!(panel.status_text(), "1 result in 1 file");
    // A regular expression that does not compile never reaches the app.
    let tree = host.tree(&panel);
    let at = node_named(&tree, "Regular expression").bounds.center();
    host.click(&mut panel, at);
    host.focus_on(panel.input().id());
    host.text(&mut panel, "(");
    assert!(host.key(&mut panel, vk::RETURN).handled);
    assert!(panel.is_invalid());
    assert!(panel.take_events().is_empty());
    assert_eq!(panel.status_text(), "Invalid regular expression");
    assert!(panel.list().is_empty(), "stale results are dropped");
    let error = host.theme.tokens.error;
    let canvas = host.paint(&panel);
    assert!(
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == error))
    );
    let tree = host.tree(&panel);
    assert!(!node_named(&tree, "Search in folder").description.is_empty());
    // Fixing the pattern searches.
    host.key(&mut panel, vk::BACK);
    host.text(&mut panel, "a");
    host.key(&mut panel, vk::RETURN);
    assert!(!panel.is_invalid());
    assert!(matches!(
        panel.take_events().as_slice(),
        [FolderSearchEvent::Search(request)] if request.pattern == "a" && request.regex
    ));
}

#[test]
fn clicking_a_match_opens_the_file_at_the_line_and_a_header_collapses_its_group() {
    let (mut host, mut panel) = setup();
    show_results(&mut host, &mut panel);
    let at = panel.list().row_rect(1).expect("row 1").center();
    assert!(host.click(&mut panel, at).handled);
    assert_eq!(
        panel.take_events(),
        vec![FolderSearchEvent::Open(FolderSearchHit {
            path: "a.md".to_owned(),
            line: 3,
            range: 2..7,
        })]
    );
    assert_eq!(panel.list().selected(), Some(1));
    // The second match of the file opens its first range.
    let at = panel.list().row_rect(2).expect("row 2").center();
    host.click(&mut panel, at);
    assert_eq!(
        panel.take_events(),
        vec![FolderSearchEvent::Open(FolderSearchHit {
            path: "a.md".to_owned(),
            line: 10,
            range: 4..9,
        })]
    );
    // Collapse a.md.
    let at = panel.list().row_rect(0).expect("header").center();
    host.click(&mut panel, at);
    assert!(panel.is_collapsed(0) && !panel.is_collapsed(1));
    assert_eq!(panel.list().rows().len(), 3);
    assert_eq!(panel.list().rows()[0].expanded, Some(false));
    assert!(panel.take_events().is_empty());
    let at = panel.list().row_rect(0).expect("header").center();
    host.click(&mut panel, at);
    assert!(!panel.is_collapsed(0));
    assert_eq!(panel.list().rows().len(), 5);
    assert!(!panel.is_collapsed(7), "unknown files are not collapsed");
}

#[test]
fn the_keyboard_moves_between_the_box_and_the_results() {
    let (mut host, mut panel) = setup();
    // Down without results stays in the box.
    assert!(!host.key(&mut panel, vk::DOWN).handled);
    show_results(&mut host, &mut panel);
    assert!(host.key(&mut panel, vk::DOWN).handled);
    assert!(host.focus.is_focused(panel.list().id()));
    assert_eq!(panel.list().selected(), Some(0));
    host.key(&mut panel, vk::DOWN);
    assert_eq!(panel.list().selected(), Some(1));
    assert!(host.key(&mut panel, vk::RETURN).handled);
    assert_eq!(
        panel.take_events(),
        vec![FolderSearchEvent::Open(FolderSearchHit {
            path: "a.md".to_owned(),
            line: 3,
            range: 2..7,
        })]
    );
    // Up on the first row goes back to the box.
    host.key(&mut panel, vk::UP);
    assert_eq!(panel.list().selected(), Some(0));
    assert!(host.focus.is_focused(panel.list().id()));
    host.key(&mut panel, vk::UP);
    assert!(host.focus.is_focused(panel.input().id()));
    // Left collapses the file under the selection, Right expands it, Enter toggles it.
    host.key(&mut panel, vk::DOWN);
    host.key(&mut panel, vk::LEFT);
    assert!(panel.is_collapsed(0));
    host.key(&mut panel, vk::LEFT);
    assert!(panel.is_collapsed(0));
    host.key(&mut panel, vk::RIGHT);
    assert!(!panel.is_collapsed(0));
    host.key(&mut panel, vk::RETURN);
    assert!(panel.is_collapsed(0));
    // Keys the list does not use are not taken; modified keys neither.
    assert!(!host.key(&mut panel, vk::F3).handled);
    assert!(!host.key_with(&mut panel, vk::DOWN, Modifiers::CTRL).handled);
    // Left and Right on a match row do nothing.
    host.key(&mut panel, vk::RETURN);
    host.key(&mut panel, vk::DOWN);
    host.key(&mut panel, vk::LEFT);
    assert!(!panel.is_collapsed(0));
    assert_eq!(panel.list().rows().len(), 5);
}

#[test]
fn the_access_tree_names_the_controls_the_status_and_every_result() {
    let (mut host, mut panel) = setup();
    show_results(&mut host, &mut panel);
    let tree = host.tree(&panel);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.role, Role::Group);
    assert_eq!(root.name, "Search in folder");
    assert_eq!(
        child_names(&tree, root.id),
        [
            "Search in folder",
            "Match case",
            "Whole word",
            "Regular expression",
            "3 results in 2 files",
            "Search results"
        ]
    );
    let list = node_named(&tree, "Search results");
    let rows: Vec<_> = tree.children(list.id).collect();
    assert_eq!(rows[0].name, "a.md, 2 results");
    assert_eq!(rows[0].state.expanded, Some(true));
    assert!(rows[0].patterns.expand_collapse);
    assert_eq!(rows[1].name, "a.md, line 3: hello world");
    assert_eq!(rows[4].name, "docs/b.md, line 1: hello");
    assert!(list.state.focusable);
    // Assistive technology expands, collapses and invokes.
    let header = rows[0].id;
    let line_id = rows[1].id;
    assert!(panel.owns(header) || panel.owns(list.id));
    let mut cx = host.cx(panel.id());
    assert!(panel.perform_child(&mut cx, header, &AccessAction::Collapse));
    assert!(panel.is_collapsed(0));
    // The rows were rebuilt: the old ids are gone.
    assert!(!panel.perform_child(&mut cx, line_id, &AccessAction::Invoke));
    let tree = host.tree(&panel);
    let rows: Vec<_> = tree
        .children(node_named(&tree, "Search results").id)
        .collect();
    let header = rows[0].id;
    let mut cx = host.cx(panel.id());
    assert!(!panel.perform_child(&mut cx, header, &AccessAction::Collapse));
    assert!(panel.perform_child(&mut cx, header, &AccessAction::Expand));
    assert!(!panel.is_collapsed(0));
    let tree = host.tree(&panel);
    let rows: Vec<_> = tree
        .children(node_named(&tree, "Search results").id)
        .collect();
    let mut cx = host.cx(panel.id());
    assert!(panel.perform_child(&mut cx, rows[2].id, &AccessAction::Select));
    assert!(panel.perform_child(&mut cx, rows[2].id, &AccessAction::Invoke));
    assert_eq!(panel.take_events().len(), 1);
    // The controls take their actions through the panel too.
    let tree = host.tree(&panel);
    let case_id = node_named(&tree, "Match case").id;
    let input_id = node_named(&tree, "Search in folder").id;
    let mut cx = host.cx(panel.id());
    assert!(panel.perform_child(&mut cx, input_id, &AccessAction::SetValue("needle".into())));
    assert_eq!(panel.input().text(), "needle");
    assert!(panel.perform_child(&mut cx, case_id, &AccessAction::Toggle));
    assert!(matches!(
        panel.take_events().as_slice(),
        [FolderSearchEvent::Search(request)] if request.case_sensitive && request.pattern == "needle"
    ));
    for name in ["Whole word", "Regular expression"] {
        let id = node_named(&host.tree(&panel), name).id;
        let mut cx = host.cx(panel.id());
        assert!(
            panel.perform_child(&mut cx, id, &AccessAction::Toggle),
            "{name}"
        );
    }
    let mut cx = host.cx(panel.id());
    assert!(!panel.perform_child(&mut cx, WidgetId::allocate(), &AccessAction::Invoke));
}

#[test]
fn the_wheel_scrolls_long_results_and_the_panel_lays_out_below_its_controls() {
    let (mut host, mut panel) = setup();
    let files: Vec<FolderSearchFile> = (0..40)
        .map(|index| FolderSearchFile {
            path: format!("f{index}.md"),
            matches: vec![line(1, "hit", &[0..3])],
        })
        .collect();
    panel.set_results(files, FolderSearchSummary::default());
    host.layout(&mut panel, BOUNDS);
    // Padding 8, query box 32, padding 8, chips 28, padding 8, status 20, padding 8.
    assert_eq!(panel.list().bounds(), Rect::new(0.0, 112.0, 300.0, 388.0));
    assert_eq!(panel.list().capacity(), 12);
    let at = panel.list().bounds().center();
    let wheel = ssw_ui::input::Event::Wheel(ssw_ui::input::WheelEvent {
        delta: ssw_ui::geometry::Point::new(0.0, -1.0),
        precise: false,
        position: at,
        mods: Modifiers::NONE,
    });
    assert!(host.send(&mut panel, wheel).handled);
    assert_eq!(panel.list().first_visible(), 3);
}
