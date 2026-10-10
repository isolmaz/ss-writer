//! Editor follow-ups: drag-and-drop of selected text, edge auto-scroll while dragging (driven by
//! a fake clock), and search highlights.

mod editor_common;

use std::time::Duration;

use editor_common::{Harness, LINE, TOP, close};
use ssw_core::edit::Selection;
use ssw_core::text::TextRange;
use ssw_ui::editor::EditorEvent;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Event, Modifiers, PointerEventKind};
use ssw_ui::widget::TimerRequest;

/// "hello" is selected in "hello world".
fn selected_hello() -> Harness {
    let mut h = Harness::new("hello world\n");
    h.view.set_selection(Selection::new(0, 5));
    h
}

fn press_on_hello(h: &mut Harness) -> Point {
    let at = h.first_line_point(2);
    h.pointer(PointerEventKind::Down, at, 1, Modifiers::NONE);
    at
}

#[test]
fn dragging_the_selection_moves_it_as_one_undo_step() {
    let mut h = selected_hello();
    press_on_hello(&mut h);
    let end = h.first_line_point(11);
    h.pointer(PointerEventKind::Move, end, 0, Modifiers::NONE);
    h.pointer(PointerEventKind::Up, end, 0, Modifiers::NONE);
    assert_eq!(h.text(), " worldhello\n");
    assert_eq!(h.selection(), Selection::new(6, 11));
    assert!(h.doc.borrow_mut().undo().expect("undo"));
    assert_eq!(h.text(), "hello world\n");
    assert!(!h.doc.borrow().can_undo(), "the move was a single step");
}

#[test]
fn ctrl_drag_copies_the_selection() {
    let mut h = selected_hello();
    press_on_hello(&mut h);
    let end = h.first_line_point(11);
    h.pointer(PointerEventKind::Move, end, 0, Modifiers::CTRL);
    h.pointer(PointerEventKind::Up, end, 0, Modifiers::CTRL);
    assert_eq!(h.text(), "hello worldhello\n");
    assert_eq!(h.selection(), Selection::new(11, 16));
}

#[test]
fn a_press_and_release_on_the_selection_places_the_caret() {
    let mut h = selected_hello();
    let at = press_on_hello(&mut h);
    h.pointer(PointerEventKind::Up, at, 0, Modifiers::NONE);
    assert_eq!(h.text(), "hello world\n");
    assert_eq!(h.selection(), Selection::caret(2));
}

#[test]
fn dropping_inside_the_dragged_text_changes_nothing() {
    let mut h = selected_hello();
    press_on_hello(&mut h);
    let inside = h.first_line_point(3);
    h.pointer(PointerEventKind::Move, inside, 0, Modifiers::NONE);
    h.pointer(PointerEventKind::Up, inside, 0, Modifiers::NONE);
    assert_eq!(h.text(), "hello world\n");
    assert_eq!(h.selection(), Selection::new(0, 5));
}

#[test]
fn releasing_outside_the_editor_reports_the_text_and_leaves_the_document() {
    let mut h = selected_hello();
    press_on_hello(&mut h);
    let outside = Point::new(500.0, 100.0);
    h.pointer(PointerEventKind::Move, outside, 0, Modifiers::NONE);
    h.events();
    h.pointer(PointerEventKind::Up, outside, 0, Modifiers::NONE);
    let events = h.events();
    assert!(
        events.contains(&EditorEvent::TextDroppedOutside {
            text: "hello".to_owned(),
            source: TextRange::new(0, 5),
            copy: false,
            position: outside,
        }),
        "{events:?}"
    );
    assert_eq!(h.text(), "hello world\n");
    // The target accepted the text: the app removes the source.
    assert!(h.view.remove_range(TextRange::new(0, 5)));
    assert_eq!(h.text(), " world\n");
}

#[test]
fn text_dropped_from_elsewhere_is_inserted_where_the_point_is() {
    let mut h = Harness::new("hello world\n");
    h.paint();
    let at = h.first_line_point(5);
    assert!(h.view.set_drop_caret(Some(at)));
    assert!(h.view.drop_text(at, "XY"));
    assert_eq!(h.text(), "helloXY world\n");
    assert_eq!(h.selection(), Selection::new(5, 7));
    assert!(!h.view.drop_text(Point::new(5_000.0, 5_000.0), "never"));
    assert!(!h.view.set_drop_caret(Some(Point::new(5_000.0, 5_000.0))));
}

fn timer_ids(h: &Harness) -> Vec<ssw_ui::services::TimerId> {
    h.requests
        .timers()
        .iter()
        .filter_map(|request| match request {
            TimerRequest::Set { id, .. } => Some(*id),
            _ => None,
        })
        .collect()
}

fn tick(h: &mut Harness, by: Duration) {
    h.clock.advance(by);
    for id in timer_ids(h) {
        h.event(Event::Timer(id));
    }
}

#[test]
fn dragging_below_the_viewport_scrolls_in_proportion_to_the_distance() {
    let text: String = (0..100).map(|i| format!("para {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    h.pointer(
        PointerEventKind::Down,
        h.first_line_point(0),
        1,
        Modifiers::NONE,
    );
    // 60 DIPs below the 200 DIP viewport: speed (60 + 12) × 20 = 1 440 DIPs per second.
    h.pointer(
        PointerEventKind::Move,
        Point::new(100.0, 260.0),
        0,
        Modifiers::NONE,
    );
    assert!(
        close(h.view.scroll_y(), 0.0),
        "nothing scrolls before a tick"
    );
    tick(&mut h, Duration::from_millis(50));
    assert!(close(h.view.scroll_y(), 72.0), "{}", h.view.scroll_y());
    assert!(
        h.selection().head > 0,
        "the selection follows the scrolled content"
    );

    // Releasing stops the timer: further clock time scrolls nothing.
    h.pointer(
        PointerEventKind::Up,
        Point::new(100.0, 260.0),
        0,
        Modifiers::NONE,
    );
    let settled = h.view.scroll_y();
    tick(&mut h, Duration::from_millis(500));
    assert!(close(h.view.scroll_y(), settled));
}

#[test]
fn a_farther_pointer_scrolls_faster_and_the_pointer_inside_does_not_scroll() {
    let text: String = (0..100).map(|i| format!("para {i}\n\n")).collect();
    let distance = |y: f32| {
        let mut h = Harness::new(&text);
        h.pointer(
            PointerEventKind::Down,
            h.first_line_point(0),
            1,
            Modifiers::NONE,
        );
        h.pointer(
            PointerEventKind::Move,
            Point::new(100.0, y),
            0,
            Modifiers::NONE,
        );
        tick(&mut h, Duration::from_millis(100));
        h.view.scroll_y()
    };
    let near = distance(210.0);
    let far = distance(400.0);
    assert!(near > 0.0 && far > 3.0 * near, "{near} {far}");
    assert!(close(distance(100.0), 0.0));
}

#[test]
fn search_highlights_unfold_and_reveal_markers_through_the_view() {
    let mut h = Harness::new("a **bold** b\n");
    h.set_caret(0);
    assert_eq!(h.painted_texts(), ["a bold b"]);
    // The match sits inside the hidden markers' element: they show.
    h.view
        .set_search_highlights(vec![TextRange::new(4, 8)], Some(0));
    assert_eq!(h.painted_texts(), ["a **bold** b"]);
    h.view.clear_search_highlights();
    assert_eq!(h.painted_texts(), ["a bold b"]);

    // A current match inside a folded section asks for an unfold; other matches do not.
    h.view.set_folds(vec![TextRange::new(8, 12)], vec![]);
    h.events();
    h.view
        .set_search_highlights(vec![TextRange::new(0, 1), TextRange::new(9, 10)], Some(0));
    assert!(
        !h.events()
            .iter()
            .any(|event| matches!(event, EditorEvent::UnfoldRequested { .. }))
    );
    h.view
        .set_search_highlights(vec![TextRange::new(0, 1), TextRange::new(9, 10)], Some(1));
    assert!(h.events().contains(&EditorEvent::UnfoldRequested {
        range: TextRange::new(9, 10),
    }));
    // An index outside the matches is ignored.
    h.view
        .set_search_highlights(vec![TextRange::new(0, 1)], Some(7));
    assert!(h.events().is_empty());
}

#[test]
fn lines_of_the_editor_are_the_configured_line_height_tall() {
    let mut h = Harness::new("one\ntwo\n");
    h.set_caret(4);
    h.arrange();
    let caret = h.view.caret_rect().expect("caret in view");
    assert!(close(caret.height, LINE), "{caret:?}");
    assert!(close(caret.y, TOP + LINE), "{caret:?}");
    h.config(|config| config.line_height = 2.0);
    h.arrange();
    let caret = h.view.caret_rect().expect("caret in view");
    assert!(close(caret.height, 32.0), "{caret:?}");
    assert!(close(caret.y, TOP + 32.0), "{caret:?}");
}
