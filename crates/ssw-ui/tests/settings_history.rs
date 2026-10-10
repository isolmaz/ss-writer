//! The version history panel (spec §4.5): the list of saved versions, the diff against the
//! current document, Restore and Close.

mod settings_support;

use settings_support::{Host, localizer};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{
    Event, Modifiers, PointerButton, PointerEvent, PointerEventKind, WheelEvent, vk,
};
use ssw_ui::shell::history::{
    DiffHunk, DiffLine, DiffLineKind, HistoryEntry, HistoryEvent, HistoryPanel,
};
use ssw_ui::widget::WidgetId;
use ssw_ui::widgets::Control;
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

/// 300 unchanged lines: taller than the panel.
fn long_hunk() -> DiffHunk {
    let lines = (0..300)
        .map(|n| {
            line(
                DiffLineKind::Context,
                Some(n),
                Some(n),
                &format!("line {n}"),
            )
        })
        .collect();
    DiffHunk {
        old_start: 0,
        old_count: 300,
        new_start: 0,
        new_count: 300,
        lines,
    }
}

/// Selects version 3 and gives it the long diff.
fn open_with_a_long_diff(motion: Motion) -> (HistoryPanel, Host) {
    let mut panel = HistoryPanel::new(localizer(), motion);
    let mut host = Host::new(PANEL);
    panel.set_entries(entries());
    host.layout(&mut panel);
    select_first(&mut panel, &mut host);
    assert!(panel.set_diff(3, vec![long_hunk()]));
    host.layout(&mut panel);
    (panel, host)
}

#[test]
fn the_panel_reports_its_motion_focus_target_and_controls() {
    let (panel, host) = open();
    assert_eq!(panel.motion(), Motion::reduced());
    let list = host.node(&panel, Role::List, "Saved versions");
    assert_eq!(panel.focus_id(), list.id);
    let restore = host.node(&panel, Role::Button, "Restore this version");
    let close = host.node(&panel, Role::Button, "Close");
    for id in [panel.id(), list.id, restore.id, close.id] {
        assert!(panel.owns(id));
    }
    assert!(!panel.owns(WidgetId::allocate()));
    let debug = format!("{panel:?}");
    assert!(debug.starts_with("HistoryPanel {"), "{debug}");
    assert!(debug.contains("entries: 3"), "{debug}");
    assert!(debug.contains("selected: None"), "{debug}");
}

#[test]
fn the_debug_form_follows_the_selection() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    let debug = format!("{panel:?}");
    assert!(debug.contains("selected: Some(3)"), "{debug}");
    assert!(debug.contains("Loading"), "{debug}");
}

#[test]
fn the_panel_measures_as_the_area_it_is_given_or_a_default_size() {
    let (mut panel, mut host) = open();
    assert_eq!(
        host.measure(&mut panel, Size::new(500.0, 700.0)),
        Size::new(500.0, 700.0)
    );
    assert_eq!(
        host.measure(&mut panel, Size::UNBOUNDED),
        Size::new(420.0, 560.0)
    );
    assert_eq!(
        host.measure(&mut panel, Size::new(f32::INFINITY, 700.0)),
        Size::new(420.0, 700.0)
    );
    assert_eq!(
        host.measure(&mut panel, Size::new(500.0, f32::INFINITY)),
        Size::new(500.0, 560.0)
    );
}

#[test]
fn nothing_is_painted_outside_the_damaged_area() {
    let (panel, host) = open();
    let away = Rect::new(600.0, 0.0, 50.0, 50.0);
    assert_eq!(host.paint_damaged(&panel, Some(away)).ops().count(), 0);
    assert!(host.paint_damaged(&panel, Some(PANEL)).ops().count() > 0);
}

#[test]
fn rows_are_not_described_before_the_panel_is_laid_out_for_them() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    // The geometry still describes the empty diff: no row can be placed yet.
    assert!(panel.set_diff(3, vec![hunk()]));
    assert!(!host.has_node(&panel, Role::ListItem, "unchanged before"));
    assert!(
        host.paint(&panel)
            .texts()
            .iter()
            .all(|t| *t != "unchanged before")
    );
    host.layout(&mut panel);
    assert!(host.has_node(&panel, Role::ListItem, "unchanged before"));
}

#[test]
fn dragging_the_diff_scroll_bar_scrolls_the_changes() {
    let (mut panel, mut host) = open_with_a_long_diff(Motion::reduced());
    let bar = host.node(&panel, Role::ScrollBar, "Changes since this version");
    let x = bar.bounds.center().x;
    let pointer = |kind: PointerEventKind, y: f32| {
        Event::Pointer(PointerEvent {
            click_count: 1,
            ..PointerEvent::mouse(kind, Some(PointerButton::Left), Point::new(x, y))
        })
    };
    let grab = bar.bounds.y + 4.0;
    assert!(
        host.event(&mut panel, pointer(PointerEventKind::Down, grab))
            .handled
    );
    host.requests.take_invalidation();
    host.event(&mut panel, pointer(PointerEventKind::Move, grab + 120.0));
    assert!(!host.requests.take_invalidation().is_clean());
    host.event(&mut panel, pointer(PointerEventKind::Up, grab + 120.0));
    let rows = row_names(&host, &panel);
    assert!(!rows.contains(&"line 0".to_owned()));
}

#[test]
fn a_notched_wheel_eases_the_diff_to_its_target_frame_by_frame() {
    let (mut panel, mut host) = open_with_a_long_diff(Motion::full());
    let inside = Point::new(200.0, 400.0);
    let notch = Event::Wheel(WheelEvent {
        delta: Point::new(0.0, -20.0),
        precise: false,
        position: inside,
        mods: Modifiers::NONE,
    });
    assert!(host.event(&mut panel, notch).handled);
    assert!(host.requests.animation_frame_requested());
    // Nothing has moved until the first frame fixes the start of the easing.
    assert!(row_names(&host, &panel).contains(&"line 0".to_owned()));
    host.requests.take_invalidation();
    for millis in [100, 200, 5_000] {
        host.event(
            &mut panel,
            Event::AnimationFrame(std::time::Duration::from_millis(millis)),
        );
    }
    assert!(!host.requests.take_invalidation().is_clean());
    assert!(!row_names(&host, &panel).contains(&"line 0".to_owned()));
}

#[test]
fn a_diff_that_fits_the_panel_takes_no_wheel_and_has_nothing_to_animate() {
    let (mut panel, mut host) = open();
    select_first(&mut panel, &mut host);
    assert!(panel.set_diff(3, vec![hunk()]));
    host.layout(&mut panel);
    let before = row_names(&host, &panel);
    host.requests.take_invalidation();
    // Nothing to scroll: the wheel over the changes is not taken and repaints nothing.
    assert!(
        !host
            .wheel(&mut panel, Point::new(200.0, 400.0), -200.0)
            .handled
    );
    assert!(host.requests.take_invalidation().is_clean());
    assert_eq!(row_names(&host, &panel), before);
    // A frame without an easing in progress changes nothing either.
    host.event(
        &mut panel,
        Event::AnimationFrame(std::time::Duration::from_millis(16)),
    );
    assert!(host.requests.take_invalidation().is_clean());
    assert_eq!(row_names(&host, &panel), before);
}

#[test]
fn assistive_technology_selects_versions_and_scrolls_the_changes() {
    let (mut panel, mut host) = open_with_a_long_diff(Motion::reduced());
    // The list takes focus; a version row is selected like a click would.
    let list = host.node(&panel, Role::List, "Saved versions");
    assert!(host.perform(&mut panel, list.id, AccessAction::Focus));
    let second = host.node(&panel, Role::ListItem, "Today 14:00 — 200 bytes");
    panel.take_events();
    assert!(host.perform(&mut panel, second.id, AccessAction::Select));
    assert_eq!(panel.selected(), Some(2));
    assert_eq!(panel.take_events(), [HistoryEvent::Selected(2)]);
    // The new version has no diff yet; answer it with the long one and scroll the bar.
    assert!(panel.set_diff(2, vec![long_hunk()]));
    host.layout(&mut panel);
    let bar = host.node(&panel, Role::ScrollBar, "Changes since this version");
    let max = bar.patterns.range_value.expect("range").max;
    assert!(host.perform(&mut panel, bar.id, AccessAction::SetRangeValue(max)));
    assert!(row_names(&host, &panel).contains(&"line 299".to_owned()));
    assert!(!host.perform(&mut panel, bar.id, AccessAction::Invoke));
}
