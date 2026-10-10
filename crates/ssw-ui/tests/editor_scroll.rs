//! Virtualization, scrolling, zoom and the writing modes.

mod editor_common;

use editor_common::{Harness, close};
use ssw_core::command::CommandId;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_ui::access::AccessAction;
use ssw_ui::canvas::DrawOp;
use ssw_ui::editor::EditorEvent;
use ssw_ui::input::{Modifiers, vk};

fn numbered_paragraphs(count: usize) -> String {
    (0..count)
        .map(|i| format!("paragraph {i}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn only_the_visible_blocks_are_laid_out() {
    let mut h = Harness::new(&numbered_paragraphs(10_000));
    // 10 000 blocks and the 9 999 blank lines between them.
    assert_eq!(h.view.item_count(), 19_999);
    let laid_out = h.view.laid_out_items();
    assert!((1..60).contains(&laid_out), "laid out {laid_out}");
    let texts = h.painted_texts();
    assert!(texts.len() < 12, "{} texts painted", texts.len());
    assert_eq!(texts[0], "paragraph 0");

    // Scrolling far away measures only what becomes visible and drops what left.
    h.view.set_scroll_y(150_000.0);
    h.arrange();
    let texts = h.painted_texts();
    let first: usize = texts[0]
        .strip_prefix("paragraph ")
        .and_then(|n| n.parse().ok())
        .expect("a numbered paragraph");
    // One paragraph and its blank line are 19.2 + 8 DIPs tall.
    assert!(
        (5510..=5516).contains(&first),
        "first visible paragraph {first}"
    );
    assert!(h.view.laid_out_items() < 150, "{}", h.view.laid_out_items());
    let visible = h.view.visible_range().expect("a visible range");
    assert!(
        visible.start > 80_000 && visible.end < 100_000,
        "{visible:?}"
    );
}

#[test]
fn estimated_heights_are_replaced_without_leaving_holes() {
    // Every paragraph wraps into three lines but is estimated as one.
    let text: String = (0..300)
        .map(|_| format!("{}\n\n", "y".repeat(100)))
        .collect();
    let mut h = Harness::new(&text);
    // 300 × (19.2 + 8) estimated DIPs: 4 000 is in the middle of the estimated content.
    h.view.set_scroll_y(4_000.0);
    h.arrange();
    let canvas = h.paint();
    let mut tops: Vec<(f32, f32)> = canvas
        .recorded()
        .iter()
        .filter(|entry| matches!(entry.op, DrawOp::Text { .. }))
        .filter_map(|entry| entry.bounds)
        .map(|bounds| (bounds.y, bounds.bottom()))
        .collect();
    tops.sort_by(|a, b| a.0.total_cmp(&b.0));
    assert!(tops.len() >= 3, "{tops:?}");
    assert!(
        tops[0].0 <= 8.01,
        "the viewport starts inside a block or its spacing: {tops:?}"
    );
    for pair in tops.windows(2) {
        let gap = pair[1].0 - pair[0].1;
        assert!(
            close(gap, 8.0),
            "paragraph spacing between blocks: {pair:?}"
        );
    }
    assert!(
        tops[tops.len() - 1].1 >= 200.0 - 8.01,
        "the viewport is filled: {tops:?}"
    );
}

#[test]
fn wheel_scrolls_by_lines_and_pixels_and_clamps() {
    let text: String = (0..60).map(|i| format!("line {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    assert!(h.wheel(1.0, false, Modifiers::NONE).handled);
    assert_eq!(h.view.scroll_y(), 0.0, "wheel up at the top does nothing");
    h.wheel(-1.0, false, Modifiers::NONE);
    assert!(
        close(h.view.scroll_y(), 84.0),
        "one notch = three body lines"
    );
    h.wheel(-30.0, true, Modifiers::NONE);
    assert!(close(h.view.scroll_y(), 114.0), "precise deltas are DIPs");
    h.wheel(-1.0e7, true, Modifiers::NONE);
    h.arrange();
    let end = h.view.scroll_y();
    assert!(end > 114.0 && end < 1.0e6, "clamped to the content: {end}");
    assert!(h.painted_texts().contains(&"line 59".to_owned()));
    h.wheel(1.0e7, true, Modifiers::NONE);
    assert_eq!(h.view.scroll_y(), 0.0);
}

#[test]
fn zoom_scales_the_text_and_is_reported() {
    let mut h = Harness::new("abc\n");
    h.wheel(1.0, false, Modifiers::CTRL);
    assert_eq!(h.view.zoom_percent(), 110);
    assert_eq!(h.events(), [EditorEvent::ZoomChanged(110)]);
    h.set_caret(1);
    h.paint();
    let caret = h.view.caret_rect().expect("caret");
    assert!(close(caret.height, 16.0 * 1.1 * 1.2), "{caret:?}");
    assert!(h.view.execute(CommandId::ViewZoomOut));
    assert_eq!(h.view.zoom_percent(), 100);
    h.view.execute(CommandId::ViewZoomIn);
    h.view.execute(CommandId::ViewZoomReset);
    assert_eq!(h.view.zoom_percent(), 100);
    assert_eq!(
        h.events(),
        [
            EditorEvent::ZoomChanged(100),
            EditorEvent::ZoomChanged(110),
            EditorEvent::ZoomChanged(100)
        ]
    );
    h.view.set_zoom_percent(1000);
    assert_eq!(h.view.zoom_percent(), 400, "clamped");
    h.wheel(-1.0, false, Modifiers::CTRL);
    assert_eq!(h.view.zoom_percent(), 390);
}

fn typewriter_document() -> (String, usize) {
    let mut text = String::new();
    for i in 0..40 {
        text.push_str(&format!("p{i}\n\n"));
    }
    let long_start = text.len();
    text.push_str(&"x".repeat(400));
    text.push_str("\n\n");
    for i in 0..40 {
        text.push_str(&format!("q{i}\n\n"));
    }
    (text, long_start)
}

fn caret_center(h: &Harness) -> f32 {
    h.paint();
    let caret = h.view.caret_rect().expect("the caret is visible");
    caret.y + caret.height / 2.0
}

#[test]
fn typewriter_mode_keeps_the_caret_line_with_a_dead_zone() {
    let (text, long_start) = typewriter_document();
    let mut h = Harness::new(&text);
    h.view.set_typewriter(true);
    assert!(h.view.typewriter());
    h.set_caret(long_start);
    // Turning the mode on asked for the caret line to be centred; the viewport is 200 tall,
    // so the line sits at 50 %.
    assert!(close(caret_center(&h), 100.0), "{}", caret_center(&h));
    let scroll = h.view.scroll_y();
    assert!(
        scroll > 1000.0,
        "the caret was far below the first screen: {scroll}"
    );
    // One 19.2 DIP line down stays inside the ±10 % (20 DIP) dead zone: the view does not move.
    h.press(vk::DOWN);
    assert!(close(caret_center(&h), 119.2), "{}", caret_center(&h));
    assert!(close(h.view.scroll_y(), scroll));
    // Two lines down leave the zone: the line is recentred.
    h.press(vk::DOWN);
    assert!(close(caret_center(&h), 100.0), "{}", caret_center(&h));
    // Moving up inside the zone keeps the view; Ctrl+L recentres anyway.
    h.press(vk::UP);
    assert!(close(caret_center(&h), 80.8), "{}", caret_center(&h));
    assert!(h.view.execute(CommandId::ViewRecenterCaret));
    assert!(close(caret_center(&h), 100.0), "{}", caret_center(&h));
}

#[test]
fn typewriter_mode_allows_overscroll_past_the_end() {
    let (text, _) = typewriter_document();
    let mut h = Harness::new(&text);
    h.view.set_scroll_y(1.0e9);
    h.arrange();
    let normal = h.view.scroll_y();
    h.view.set_typewriter(true);
    h.arrange();
    h.view.set_scroll_y(1.0e9);
    h.arrange();
    assert!(
        close(h.view.scroll_y() - normal, 100.0),
        "half a viewport of overscroll"
    );
}

#[test]
fn without_typewriter_the_caret_scrolls_into_view_minimally() {
    let text: String = (0..60).map(|i| format!("line {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    let target = text.find("line 50").expect("paragraph");
    h.view.go_to(Selection::caret(target));
    h.paint();
    let caret = h
        .view
        .caret_rect()
        .expect("go_to scrolled the caret into view");
    assert!(caret.y >= 0.0 && caret.bottom() <= 200.0, "{caret:?}");
    let scroll = h.view.scroll_y();
    h.press(vk::LEFT);
    h.press(vk::RIGHT);
    h.paint();
    assert!(
        close(h.view.scroll_y(), scroll),
        "the caret stays visible: nothing to do"
    );
}

#[test]
fn accessibility_scrolls_to_a_percentage() {
    let text: String = (0..60).map(|i| format!("line {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    assert!(h.perform(&AccessAction::ScrollTo {
        horizontal: None,
        vertical: Some(50.0),
    }));
    let scroll = h.access().patterns.scroll.expect("scroll pattern");
    assert!(scroll.vertically_scrollable);
    assert!((scroll.vertical_percent - 50.0).abs() < 0.5, "{scroll:?}");
    assert!(h.perform(&AccessAction::Focus));
    assert!(!h.perform(&AccessAction::Invoke));
}

#[test]
fn changes_made_elsewhere_to_the_shared_document_are_laid_out() {
    let h = Harness::new("old\n");
    h.paint();
    let before = h.view.item_count();
    h.doc
        .borrow_mut()
        .apply(Transaction::single(
            Edit::insert(0, "NEW\n\n"),
            Selection::caret(0),
            Selection::caret(5),
        ))
        .expect("apply");
    assert_eq!(h.painted_texts(), ["NEW", "old"]);
    assert_eq!(h.view.item_count(), before + 2);
    assert_eq!(h.selection(), Selection::caret(5));
}
