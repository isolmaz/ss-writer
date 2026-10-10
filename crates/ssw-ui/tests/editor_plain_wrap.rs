//! Plain-text documents without word wrap (spec §4.1): one row per line, sideways scrolling
//! with the wheel, caret and accessibility, and switching the wrapping while a document is open.

mod editor_common;

use editor_common::{COLUMN_LEFT, Harness, close};
use ssw_core::edit::Selection;
use ssw_ui::access::AccessAction;
use ssw_ui::canvas::DrawOp;
use ssw_ui::editor::DocumentKind;
use ssw_ui::geometry::Rect;
use ssw_ui::input::{Modifiers, vk};

/// Characters of a long line: 800 DIPs at 8 DIPs each, far wider than the 352 DIP column.
const LONG: usize = 100;
/// How far the long line scrolls: its width minus the 352 DIPs shown.
const FAR: f32 = 448.0;

fn plain(text: &str, wrap: bool) -> Harness {
    let mut h = Harness::new(text);
    h.config(|config| {
        config.document_kind = DocumentKind::PlainText;
        config.wrap = wrap;
    });
    h
}

/// A document of `count` long lines.
fn long_lines(count: usize) -> String {
    format!("{}\n", "x".repeat(LONG)).repeat(count)
}

/// The window bounds of every text drawn, top to bottom.
fn text_bounds(h: &Harness) -> Vec<Rect> {
    h.paint()
        .recorded()
        .iter()
        .filter(|entry| matches!(entry.op, DrawOp::Text { .. }))
        .filter_map(|entry| entry.bounds)
        .collect()
}

/// Where the drawn lines start horizontally.
fn text_x(h: &Harness) -> f32 {
    text_bounds(h)[0].x
}

#[test]
fn an_unwrapped_line_keeps_one_row_and_a_wrapped_one_takes_several() {
    // The distance between two short lines is one row.
    let short = plain("a\nb\n", true);
    let tops = text_bounds(&short);
    let row = tops[1].y - tops[0].y;
    assert!(row > 0.0, "{tops:?}");

    let distance = |wrap: bool| {
        let h = plain(&format!("{}\nb\n", "x".repeat(LONG)), wrap);
        let tops = text_bounds(&h);
        tops[1].y - tops[0].y
    };
    assert!(close(distance(false), row), "one row without wrapping");
    assert!(
        distance(true) > 1.5 * row,
        "several rows with wrapping: {} against {row}",
        distance(true)
    );
}

#[test]
fn shift_and_the_wheel_scroll_unwrapped_text_sideways_and_otherwise_up_and_down() {
    let mut h = plain(&long_lines(40), false);
    assert!(close(text_x(&h), COLUMN_LEFT));

    // Shift turns a vertical notch (three lines of 28 DIPs) into a sideways one.
    h.wheel(-1.0, false, Modifiers::SHIFT);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - 84.0), "{}", text_x(&h));
    assert_eq!(h.view.scroll_y(), 0.0, "the rows stay");
    h.wheel(1.0, false, Modifiers::SHIFT);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT));
    h.wheel(1.0, false, Modifiers::SHIFT);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT), "not before the line start");

    // Without Shift the same notch scrolls the rows.
    h.wheel(-1.0, false, Modifiers::NONE);
    h.arrange();
    assert!(close(h.view.scroll_y(), 84.0));
    assert!(close(text_x(&h), COLUMN_LEFT), "the columns stay");

    // Wrapped text has no sideways scroll: Shift changes nothing about the notch.
    let mut wrapped = plain(&long_lines(40), true);
    // A window paints before the user can wheel: the lines are measured (three rows each, not
    // the one-row estimate) and the scroll position is anchored to measured heights.
    assert!(close(text_x(&wrapped), COLUMN_LEFT));
    wrapped.wheel(-1.0, false, Modifiers::SHIFT);
    wrapped.arrange();
    assert!(close(wrapped.view.scroll_y(), 84.0));
    assert!(close(text_x(&wrapped), COLUMN_LEFT));
}

#[test]
fn the_caret_is_brought_back_into_view_in_both_directions() {
    let mut h = plain(&long_lines(1), false);
    h.press(vk::END);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - FAR));

    // Home scrolls back to the line start.
    h.press(vk::HOME);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT));
    let caret = h.view.caret_rect().expect("the caret is in view");
    assert!(close(caret.x, COLUMN_LEFT), "{caret:?}");

    // A jump into the middle of the line, from the far end, scrolls just far enough left.
    h.press(vk::END);
    h.arrange();
    h.view.go_to(Selection::caret(10));
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - 56.0), "{}", text_x(&h));
    let caret = h.view.caret_rect().expect("the caret is in view");
    assert!((0.0..=400.0).contains(&caret.x), "{caret:?}");
}

#[test]
fn switching_the_wrapping_starts_the_view_at_the_line_start_again() {
    let mut h = plain(&long_lines(1), false);
    h.press(vk::END);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - FAR));

    h.config(|config| config.wrap = true);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT), "wrapped text has no offset");
    h.config(|config| config.wrap = false);
    h.arrange();
    assert!(
        close(text_x(&h), COLUMN_LEFT),
        "the sideways offset of the old setting is gone"
    );
    // The same setting again changes nothing.
    h.press(vk::END);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - FAR));
    h.config(|config| config.wrap = false);
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - FAR));
}

#[test]
fn accessibility_reports_and_sets_the_sideways_scroll_of_unwrapped_text() {
    let mut h = plain(&long_lines(40), false);
    let scroll = h.access().patterns.scroll.expect("scroll pattern");
    assert!(scroll.horizontally_scrollable && scroll.vertically_scrollable);
    assert!(close(scroll.horizontal_percent, 0.0), "{scroll:?}");
    // 352 of the 800 DIPs of a line are visible.
    assert!(close(scroll.horizontal_view_percent, 44.0), "{scroll:?}");

    // Both axes at once.
    assert!(h.perform(&AccessAction::ScrollTo {
        horizontal: Some(50.0),
        vertical: Some(25.0),
    }));
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT - FAR / 2.0), "{}", text_x(&h));
    let scroll = h.access().patterns.scroll.expect("scroll pattern");
    assert!(close(scroll.horizontal_percent, 50.0), "{scroll:?}");
    let vertical = scroll.vertical_percent;
    assert!(vertical > 1.0, "the rows scrolled as well: {scroll:?}");

    // Only sideways; percentages beyond the range stop at the end.
    assert!(h.perform(&AccessAction::ScrollTo {
        horizontal: Some(250.0),
        vertical: None,
    }));
    h.arrange();
    let scroll = h.access().patterns.scroll.expect("scroll pattern");
    assert!(close(scroll.horizontal_percent, 100.0), "{scroll:?}");
    assert!(
        (scroll.vertical_percent - vertical).abs() < 0.5,
        "the rows stay: {scroll:?} against {vertical}"
    );

    // Nothing asked, nothing done.
    assert!(!h.perform(&AccessAction::ScrollTo {
        horizontal: None,
        vertical: None,
    }));
}

#[test]
fn wrapped_text_reports_no_sideways_scroll_and_refuses_it() {
    let mut h = plain(&long_lines(40), true);
    let scroll = h.access().patterns.scroll.expect("scroll pattern");
    assert!(!scroll.horizontally_scrollable && scroll.vertically_scrollable);
    assert!(close(scroll.horizontal_percent, 0.0));
    assert!(close(scroll.horizontal_view_percent, 100.0));
    assert!(!h.perform(&AccessAction::ScrollTo {
        horizontal: Some(50.0),
        vertical: None,
    }));
    h.arrange();
    assert!(close(text_x(&h), COLUMN_LEFT));
    // The vertical part of the same request still applies.
    assert!(h.perform(&AccessAction::ScrollTo {
        horizontal: Some(50.0),
        vertical: Some(50.0),
    }));
    assert!(close(text_x(&h), COLUMN_LEFT));
}
