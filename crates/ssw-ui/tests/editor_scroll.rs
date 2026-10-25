//! Virtualization, scrolling, zoom and the writing modes.

mod editor_common;

use editor_common::{Harness, LINE, close};
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
    let text = numbered_paragraphs(10_000);
    let mut h = Harness::new(&text);
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
    // One paragraph and its blank line (the 8 DIP paragraph spacing) are LINE + 8 DIPs tall.
    let expected = (150_000.0 / (LINE + 8.0)) as usize;
    assert!(
        (expected - 3..=expected + 3).contains(&first),
        "first visible paragraph {first}, expected near {expected}"
    );
    assert!(h.view.laid_out_items() < 150, "{}", h.view.laid_out_items());
    // The visible source range holds the first painted paragraph and spans only the handful
    // of paragraphs (each "paragraph N" plus its separator is at most 20 bytes) that fit the
    // 200 DIP viewport.
    let visible = h.view.visible_range().expect("a visible range");
    let at = text
        .find(&format!("paragraph {first}\n"))
        .expect("the painted paragraph is in the text");
    assert!(visible.start <= at && at < visible.end, "{visible:?} {at}");
    assert!(visible.end - visible.start < 12 * 20, "{visible:?}");
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
    assert!(close(caret.height, LINE * 1.1), "{caret:?}");
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

#[test]
fn zooming_keeps_the_same_text_at_the_top_of_the_view() {
    // Every paragraph wraps into three lines but is estimated as one, so rebuilding the heights
    // after a zoom would put a much later paragraph under the scroll offset.
    let text: String = (0..300)
        .map(|i| format!("p{i:03} {}

", "y".repeat(100)))
        .collect();
    let mut h = Harness::new(&text);
    h.view.set_scroll_y(4_000.0);
    h.arrange();
    let top = |h: &Harness| {
        let texts = h.painted_texts();
        texts[0][..4].to_owned()
    };
    let before = top(&h);
    assert_ne!(before, "p000", "the view is scrolled into the document");
    assert!(h.view.execute(CommandId::ViewZoomIn));
    h.arrange();
    assert_eq!(top(&h), before);
    assert!(h.view.execute(CommandId::ViewZoomOut));
    assert!(h.view.execute(CommandId::ViewZoomOut));
    h.arrange();
    assert_eq!(top(&h), before);
}

#[test]
fn zooming_at_the_top_keeps_the_document_start() {
    let text = numbered_paragraphs(50);
    let mut h = Harness::new(&text);
    h.view.execute(CommandId::ViewZoomOut);
    h.arrange();
    assert_eq!(h.view.scroll_y(), 0.0);
    assert_eq!(h.painted_texts()[0], "paragraph 0");
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
    // A ±20 % dead zone of the 200 DIP viewport is ±40 DIPs: wider than one line (LINE), and
    // narrower than two.
    h.config(|config| config.typewriter_dead_zone = 0.2);
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
    // One line down stays inside the dead zone: the view does not move.
    h.press(vk::DOWN);
    assert!(
        close(caret_center(&h), 100.0 + LINE),
        "{}",
        caret_center(&h)
    );
    assert!(close(h.view.scroll_y(), scroll));
    // Two lines down leave the zone: the line is recentred.
    h.press(vk::DOWN);
    assert!(close(caret_center(&h), 100.0), "{}", caret_center(&h));
    // Moving up inside the zone keeps the view; Ctrl+L recentres anyway.
    h.press(vk::UP);
    assert!(
        close(caret_center(&h), 100.0 - LINE),
        "{}",
        caret_center(&h)
    );
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
    // Turning the mode off again takes the overscroll away.
    h.view.set_typewriter(false);
    assert!(!h.view.typewriter());
    h.arrange();
    h.view.set_scroll_y(1.0e9);
    h.arrange();
    assert!(
        close(h.view.scroll_y(), normal),
        "{} against {normal}",
        h.view.scroll_y()
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

#[test]
fn scroll_extent_is_the_range_the_editor_accepts() {
    let text: String = (0..60).map(|i| format!("line {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    let extent = h.view.scroll_extent();
    assert_eq!(extent.viewport, 200.0, "the viewport is the editor height");
    assert!(extent.content > 200.0, "{extent:?}");
    assert_eq!(extent.end_overscroll, 0.0);
    // Scrolling far past the end is clamped to the largest offset the extent reports.
    h.view.set_scroll_y(1.0e9);
    h.arrange();
    h.arrange();
    let extent = h.view.scroll_extent();
    assert!(
        close(h.view.scroll_y(), extent.max_offset() as f32),
        "{} against {extent:?}",
        h.view.scroll_y()
    );
    // Typewriter mode adds half a viewport of room past the end (typewriter position 0.5).
    h.view.set_typewriter(true);
    h.arrange();
    h.view.set_scroll_y(1.0e9);
    h.arrange();
    h.arrange();
    let typewriter = h.view.scroll_extent();
    assert!(
        close(h.view.scroll_y(), typewriter.max_offset() as f32),
        "{} against {typewriter:?}",
        h.view.scroll_y()
    );
    assert!(
        close(
            (typewriter.max_offset() - extent.max_offset()) as f32,
            100.0
        ),
        "{extent:?} then {typewriter:?}"
    );
}

#[test]
fn scroll_extent_of_a_short_document_cannot_scroll() {
    let mut h = Harness::new("abc\n");
    h.view.set_typewriter(true);
    h.arrange();
    let extent = h.view.scroll_extent();
    // 24 + one or two 28 DIP lines + 48 DIPs of padding, plus 100 DIPs of typewriter room,
    // barely exceed the 200 DIP viewport: the overscroll is not added on top of the viewport.
    assert!(
        extent.max_offset() <= f64::from(LINE),
        "a short document has next to nothing to scroll: {extent:?}"
    );
    h.view.set_scroll_y(1.0e9);
    h.arrange();
    assert!(close(h.view.scroll_y(), extent.max_offset() as f32));
}
