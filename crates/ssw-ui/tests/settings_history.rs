//! The version history panel (spec §4.5): the list of saved versions, the diff against the
//! current document, Restore and Close.

mod settings_support;

use settings_support::{Host, localizer};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{Modifiers, vk};
use ssw_ui::shell::history::{
    DiffHunk, DiffLine, DiffLineKind, HistoryEntry, HistoryEvent, HistoryPanel,
};
use ssw_ui::widgets::anim::Motion;

const PANEL: Rect = Rect::new(0.0, 0.0, 420.0, 640.0);

fn entries() -> Vec<HistoryEntry> {
    vec![
        HistoryEntry {
            id: 3,
            time: "Today 14:30".to_owned(),
            size: 1536,
        },
        HistoryEntry {
            id: 2,
            time: "Today 14:00".to_owned(),
            size: 200,
        },
        HistoryEntry {
            id: 1,
            time: "Yesterday".to_owned(),
            size: 3_000_000,
        },
    ]
}

fn open_with(width: f32) -> (HistoryPanel, Host) {
    let mut panel = HistoryPanel::new(localizer(), Motion::reduced());
    let mut host = Host::new(Rect::new(0.0, 0.0, width, PANEL.height));
    panel.set_entries(entries());
    host.layout(&mut panel);
    (panel, host)
}

fn open() -> (HistoryPanel, Host) {
    open_with(PANEL.width)
}

fn line(kind: DiffLineKind, old: Option<usize>, new: Option<usize>, text: &str) -> DiffLine {
    DiffLine {
        kind,
        old_line: old,
        new_line: new,
        text: text.to_owned(),
    }
}

fn hunk() -> DiffHunk {
    DiffHunk {
        old_start: 2,
        old_count: 4,
        new_start: 2,
        new_count: 5,
        lines: vec![
            line(DiffLineKind::Context, Some(2), Some(2), "unchanged before"),
            line(DiffLineKind::Removed, Some(3), None, "old wording"),
            line(DiffLineKind::Added, None, Some(3), "new wording"),
            line(DiffLineKind::Added, None, Some(4), "another new line"),
            line(DiffLineKind::Context, Some(4), Some(5), "unchanged after"),
        ],
    }
}

fn select_first(panel: &mut HistoryPanel, host: &mut Host) {
    let row = host.node(panel, Role::ListItem, "Today 14:30 — 1.5 KB");
    host.click(panel, row.bounds.center());
}

fn row_names(host: &Host, panel: &HistoryPanel) -> Vec<String> {
    host.tree(panel)
        .nodes()
        .iter()
        .filter(|node| node.role == Role::ListItem)
        .map(|node| node.name.clone())
        .collect()
}

#[test]
fn versions_are_listed_with_time_and_size() {
    let (panel, host) = open();
    assert_eq!(
        row_names(&host, &panel),
        [
            "Today 14:30 — 1.5 KB",
            "Today 14:00 — 200 bytes",
            "Yesterday — 2.9 MB"
        ]
    );
    assert_eq!(panel.entries().len(), 3);
    assert_eq!(panel.selected(), None);
    // Nothing is selected, so the changes area explains what to do.
    assert!(host.has_node(&panel, Role::Text, "Select a version to see what changed."));
    assert!(
        host.node(&panel, Role::Button, "Restore this version")
            .state
            .disabled
    );
}

#[test]
fn an_empty_history_says_so() {
    let mut panel = HistoryPanel::new(localizer(), Motion::reduced());
    let mut host = Host::new(PANEL);
    host.layout(&mut panel);
    assert!(host.has_node(
        &panel,
        Role::Text,
        "There are no saved versions of this document yet."
    ));
    assert!(
        host.paint(&panel)
            .texts()
            .contains(&"There are no saved versions of this document yet.")
    );
}

#[test]
fn selecting_a_version_requests_its_diff() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    assert_eq!(panel.selected(), Some(3));
    assert_eq!(panel.take_events(), vec![HistoryEvent::Selected(3)]);
    assert!(host.has_node(&panel, Role::Text, "Comparing…"));
    // Answers for another version are dropped.
    assert!(!panel.set_diff(2, vec![hunk()]));
    assert!(host.has_node(&panel, Role::Text, "Comparing…"));
    assert!(panel.set_diff(3, vec![hunk()]));
    host.layout(&mut panel);
    assert!(!host.has_node(&panel, Role::Text, "Comparing…"));
    // Selecting it again changes nothing.
    panel.select(3);
    assert!(panel.take_events().is_empty());
    // Programmatic selection behaves like a click.
    panel.select(2);
    assert_eq!(panel.take_events(), vec![HistoryEvent::Selected(2)]);
    panel.select(99);
    assert_eq!(panel.selected(), Some(2));
}

#[test]
fn the_diff_shows_hunks_with_numbers_and_colors() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    panel.set_diff(3, vec![hunk()]);
    host.layout(&mut panel);
    let names = row_names(&host, &panel);
    assert_eq!(
        names[3..],
        [
            "Lines 3–6 before, 3–7 now",
            "unchanged before",
            "Removed: old wording",
            "Added: new wording",
            "Added: another new line",
            "unchanged after"
        ]
    );
    let canvas = host.paint(&panel);
    let texts = canvas.texts();
    for expected in [
        "Version history",
        "Changes since this version",
        "Lines 3–6 before, 3–7 now",
        "unchanged before",
        "old wording",
        "new wording",
        "another new line",
        "unchanged after",
        "Restore this version",
    ] {
        assert!(texts.contains(&expected), "{expected:?} is drawn");
    }
    // Line numbers are 1-based; added lines have none on the old side.
    for number in ["3", "4", "5", "6"] {
        assert!(texts.contains(&number), "line number {number}");
    }
    let tokens = &host.theme.tokens;
    let count = |color| {
        canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRect { color: c, .. } if *c == color))
            .count()
    };
    assert_eq!(count(tokens.error.with_alpha(0.16)), 1);
    assert_eq!(count(tokens.success.with_alpha(0.16)), 2);
    assert!(
        count(tokens.code_bg) >= 1,
        "the hunk header has its own background"
    );
    // The markers are drawn for changed lines.
    assert!(texts.contains(&"+") && texts.contains(&"-"));
}

#[test]
fn an_identical_version_says_so() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    panel.set_diff(3, Vec::new());
    host.layout(&mut panel);
    assert!(host.has_node(
        &panel,
        Role::Text,
        "This version is identical to the current document."
    ));
}

#[test]
fn restore_and_close_are_events() {
    let (mut panel, mut host) = open();
    let restore = host.node(&panel, Role::Button, "Restore this version");
    assert!(!host.perform(&mut panel, restore.id, AccessAction::Invoke));
    select_first(&mut panel, &mut host);
    let restore = host.node(&panel, Role::Button, "Restore this version");
    assert!(!restore.state.disabled);
    host.click(&mut panel, restore.bounds.center());
    let close = host.node(&panel, Role::Button, "Close");
    host.perform(&mut panel, close.id, AccessAction::Invoke);
    assert_eq!(
        panel.take_events(),
        [
            HistoryEvent::Selected(3),
            HistoryEvent::Restore(3),
            HistoryEvent::Close
        ]
    );
    // Esc closes the panel as well.
    host.key(&mut panel, vk::ESCAPE, Modifiers::NONE);
    assert_eq!(panel.take_events(), [HistoryEvent::Close]);
}

#[test]
fn replacing_the_versions_keeps_or_clears_the_selection() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    panel.take_events();
    panel.set_diff(3, vec![hunk()]);
    // A new snapshot arrived: the selected version is still listed.
    let mut newer = entries();
    newer.insert(
        0,
        HistoryEntry {
            id: 4,
            time: "Today 15:00".to_owned(),
            size: 10,
        },
    );
    panel.set_entries(newer);
    host.layout(&mut panel);
    assert_eq!(panel.selected(), Some(3));
    assert!(
        !host
            .node(&panel, Role::Button, "Restore this version")
            .state
            .disabled
    );
    assert!(row_names(&host, &panel).contains(&"Today 15:00 — 10 bytes".to_owned()));
    // The selected version expired.
    panel.set_entries(vec![entries().remove(1)]);
    host.layout(&mut panel);
    assert_eq!(panel.selected(), None);
    assert!(
        host.node(&panel, Role::Button, "Restore this version")
            .state
            .disabled
    );
    assert!(host.has_node(&panel, Role::Text, "Select a version to see what changed."));
}

#[test]
fn a_long_diff_scrolls_and_only_describes_the_rows_in_view() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    let lines: Vec<DiffLine> = (0..300)
        .map(|n| {
            line(
                DiffLineKind::Context,
                Some(n),
                Some(n),
                &format!("line {n}"),
            )
        })
        .collect();
    panel.set_diff(
        3,
        vec![DiffHunk {
            old_start: 0,
            old_count: 300,
            new_start: 0,
            new_count: 300,
            lines,
        }],
    );
    host.layout(&mut panel);
    let top = row_names(&host, &panel);
    assert!(top.len() < 40, "{} rows described", top.len());
    assert!(top.contains(&"line 0".to_owned()));
    assert!(!top.contains(&"line 299".to_owned()));
    assert!(host.has_node(&panel, Role::ScrollBar, "Changes since this version"));
    let inside = Point::new(200.0, 400.0);
    host.wheel(&mut panel, inside, -200.0);
    let scrolled = row_names(&host, &panel);
    assert!(!scrolled.contains(&"line 0".to_owned()));
    assert!(scrolled.len() < 40);
    host.wheel(&mut panel, inside, -100_000.0);
    assert!(row_names(&host, &panel).contains(&"line 299".to_owned()));
    // Wheeling away from the changes does nothing.
    let before = row_names(&host, &panel);
    host.wheel(&mut panel, Point::new(200.0, 20.0), 500.0);
    assert_eq!(row_names(&host, &panel), before);
    // A new diff starts at the top again.
    panel.select(2);
    panel.set_diff(2, vec![hunk()]);
    host.layout(&mut panel);
    assert!(row_names(&host, &panel).contains(&"unchanged before".to_owned()));
}

#[test]
fn long_lines_wrap_to_the_panel_width() {
    let long = "word ".repeat(40);
    let hunks = vec![DiffHunk {
        old_start: 0,
        old_count: 0,
        new_start: 0,
        new_count: 1,
        lines: vec![line(DiffLineKind::Added, None, Some(0), long.trim_end())],
    }];
    let heights = |width: f32| {
        let (mut panel, mut host) = open_with(width);
        select_first(&mut panel, &mut host);
        panel.set_diff(3, hunks.clone());
        host.layout(&mut panel);
        let name = format!("Added: {}", long.trim_end());
        host.node(&panel, Role::ListItem, &name).bounds.height
    };
    let wide = heights(900.0);
    let narrow = heights(420.0);
    assert!(wide > 20.0, "{wide}");
    assert!(narrow > wide + 10.0, "{narrow} vs {wide}");
}

#[test]
fn the_panel_is_one_access_tree() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    panel.set_diff(3, vec![hunk()]);
    host.layout(&mut panel);
    let tree = host.tree(&panel);
    let root = tree.root().unwrap_or_else(|| panic!("no root"));
    assert_eq!(root.role, Role::Pane);
    assert_eq!(root.name, "Version history");
    assert_eq!(tree.walk().len(), tree.len());
    let canvas = host.paint(&panel);
    assert!(canvas.is_balanced());
}
