//! DirectWrite layout through the public `TextLayoutEngine` contract: clusters, UTF-8 offsets,
//! wrapping, line metrics, fonts and inline objects, with real DirectWrite objects.

use ssw_ui::geometry::{Point, Size};
use ssw_ui::text::{
    FontWeight, InlineObject, StyledRange, TextLayout, TextLayoutEngine, TextStyle,
};
use ssw_win::com::ComGuard;
use ssw_win::dwrite::{DWriteEngine, DWriteLayout, FailedLayout};

fn engine() -> (ComGuard, DWriteEngine) {
    let com = ComGuard::apartment_threaded().unwrap();
    (com, DWriteEngine::new().unwrap())
}

fn segoe(size: f32) -> TextStyle {
    TextStyle::new("Segoe UI", size)
}

/// The distinct cluster start offsets met when sweeping a horizontal probe through the line,
/// in the order they are met, with the cluster length reported for each.
fn clusters_by_sweep(layout: &dyn TextLayout) -> Vec<(usize, usize)> {
    let y = layout.size().height / 2.0;
    let mut found: Vec<(usize, usize)> = Vec::new();
    let mut x = 0.0f32;
    while x < layout.size().width {
        let hit = layout.hit_test_point(Point::new(x, y));
        assert!(hit.inside, "x = {x} is inside the laid-out text");
        if found.last().map(|&(offset, _)| offset) != Some(hit.offset) {
            found.push((hit.offset, hit.length));
        }
        x += 0.25;
    }
    found
}

#[test]
fn clusters_follow_character_boundaries_in_every_script() {
    let (_com, engine) = engine();
    // (label, text, byte offset of every cluster)
    let samples: [(&str, &str, &[usize]); 5] = [
        (
            "turkish dotted and dotless i, cedilla, breve, umlauts",
            "İıŞşĞğÇçÖöÜü",
            &[0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22],
        ),
        ("emoji outside the BMP", "a😀b", &[0, 1, 5]),
        ("cjk", "日本語", &[0, 3, 6]),
        ("combining marks", "e\u{301}a\u{308}z", &[0, 3, 6]),
        (
            "mixed scripts",
            "çay 茶 ☕ x",
            &[0, 2, 3, 4, 5, 8, 9, 12, 13],
        ),
    ];
    for (label, text, starts) in samples {
        let layout = engine.layout(text, &segoe(18.0), &[], &[], f32::INFINITY);
        assert_eq!(layout.text(), text, "{label}");
        assert_eq!(layout.line_count(), 1, "{label}");
        let found = clusters_by_sweep(&*layout);
        let offsets: Vec<usize> = found.iter().map(|&(offset, _)| offset).collect();
        // Spaces are clusters of their own; the sweep may hit each start exactly once.
        assert_eq!(offsets, starts, "{label}: cluster starts");
        for (index, &(offset, length)) in found.iter().enumerate() {
            let next = starts.get(index + 1).copied().unwrap_or(text.len());
            assert_eq!(
                offset + length,
                next,
                "{label}: cluster at {offset} spans to {next}"
            );
            assert!(
                text.is_char_boundary(offset),
                "{label}: {offset} is a char boundary"
            );
        }
        // Carets advance monotonically and the end caret sits at the right edge.
        let mut previous = f32::NEG_INFINITY;
        for &offset in starts {
            let x = layout.caret_rect(offset, false).x;
            assert!(
                x > previous,
                "{label}: caret at {offset} moves right ({x} > {previous})"
            );
            previous = x;
        }
        let end = layout.caret_rect(text.len(), false);
        assert!(end.x > previous, "{label}");
        assert!(
            (end.x - layout.size().width).abs() < 1.5,
            "{label}: {end:?} {:?}",
            layout.size()
        );
    }
}

#[test]
fn hit_testing_inverts_caret_positions() {
    let (_com, engine) = engine();
    let text = "Işık günü: 日本語 😀";
    let layout = engine.layout(text, &segoe(20.0), &[], &[], f32::INFINITY);
    let mut boundaries: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    boundaries.push(text.len());
    for &offset in &boundaries {
        let leading = layout.caret_rect(offset, false);
        assert_eq!(leading.width, 0.0, "carets have no width");
        assert!(leading.height > 0.0);
        if offset == text.len() {
            // Past the last cluster the hit clamps to the trailing edge of the final cluster.
            let hit = layout.hit_test_point(Point::new(leading.x + 40.0, leading.y + 2.0));
            assert!(!hit.inside);
            assert_eq!(hit.insertion_offset(), text.len());
            continue;
        }
        let hit = layout.hit_test_point(Point::new(
            leading.x + 0.5,
            leading.y + leading.height / 2.0,
        ));
        assert_eq!(hit.offset, offset, "leading edge of {offset}");
        assert!(!hit.trailing);
        assert_eq!(hit.insertion_offset(), offset);
    }
}

#[test]
fn offsets_inside_astral_characters_are_never_reported() {
    let (_com, engine) = engine();
    let text = "😀😀";
    let layout = engine.layout(text, &segoe(24.0), &[], &[], f32::INFINITY);
    let found = clusters_by_sweep(&*layout);
    assert_eq!(
        found.iter().map(|&(offset, _)| offset).collect::<Vec<_>>(),
        vec![0, 4]
    );
    assert!(found.iter().all(|&(_, length)| length == 4));
    let rects = layout.range_rects(0..4);
    assert_eq!(rects.len(), 1);
    let both = layout.range_rects(0..8);
    assert_eq!(both.len(), 1);
    assert!(both[0].width > rects[0].width * 1.8, "{both:?} {rects:?}");
    assert!((both[0].right() - layout.caret_rect(8, false).x).abs() < 1.5);
}

#[test]
fn the_bundled_monospace_font_has_equal_advances() {
    let (_com, engine) = engine();
    assert_eq!(engine.bundled_font_count(), 1);
    assert!(engine.has_family("Cascadia Mono"));
    let style = TextStyle::new("Cascadia Mono", 20.0);
    let widths: Vec<f32> = ["iiii", "MMMM", "w.w.", "ğİşı"]
        .iter()
        .map(|text| {
            engine
                .layout(text, &style, &[], &[], f32::INFINITY)
                .size()
                .width
        })
        .collect();
    for width in &widths {
        assert!((width - widths[0]).abs() < 0.5, "{widths:?}");
    }
    let layout = engine.layout("iiii", &style, &[], &[], f32::INFINITY);
    let advance = layout.caret_rect(1, false).x;
    assert!(
        advance > 8.0 && advance < 14.0,
        "advance {advance} for 20 dip Cascadia Mono"
    );
    for column in 1..4 {
        let x = layout.caret_rect(column, false).x;
        assert!(
            (x - advance * column as f32).abs() < 0.5,
            "column {column}: {x}"
        );
    }
}

#[test]
fn proportional_fonts_differ_from_monospace() {
    let (_com, engine) = engine();
    let proportional = engine
        .layout("iiii", &segoe(20.0), &[], &[], f32::INFINITY)
        .size()
        .width;
    let wide = engine
        .layout("MMMM", &segoe(20.0), &[], &[], f32::INFINITY)
        .size()
        .width;
    assert!(wide > proportional * 2.0, "{wide} vs {proportional}");
}

#[test]
fn wrapping_produces_contiguous_lines_that_fit() {
    let (_com, engine) = engine();
    let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
    let mut previous_lines = 0;
    for width in [320.0f32, 200.0, 120.0, 90.0] {
        let layout = engine.layout(text, &segoe(16.0), &[], &[], width);
        let count = layout.line_count();
        assert!(
            count >= previous_lines,
            "narrower layouts never have fewer lines ({width})"
        );
        previous_lines = count;
        let mut expected_start = 0;
        let mut previous_top = f32::NEG_INFINITY;
        for index in 0..count {
            let line = layout.line_metrics(index).unwrap();
            assert_eq!(line.start, expected_start, "line {index} at width {width}");
            assert!(line.end > line.start);
            assert_eq!(line.newline_len, 0, "soft wraps carry no break");
            assert!(line.top > previous_top && line.height > 0.0);
            assert!(line.baseline > line.top && line.baseline < line.top + line.height);
            assert_eq!(layout.line_for_offset(line.start), index);
            previous_top = line.top;
            expected_start = line.end;
        }
        assert_eq!(
            expected_start,
            text.len(),
            "the lines cover the whole text at {width}"
        );
        assert!(layout.line_metrics(count).is_none());
        assert!(
            layout.size().width <= width + 0.5,
            "{:?} at {width}",
            layout.size()
        );
        let rects = layout.range_rects(0..text.len());
        assert_eq!(rects.len(), count, "one range rectangle per line");
    }
    assert!(previous_lines >= 4);
}

#[test]
fn hard_breaks_report_their_byte_length() {
    let (_com, engine) = engine();
    let layout = engine.layout("a\r\nb\nc", &segoe(16.0), &[], &[], f32::INFINITY);
    let lines: Vec<(usize, usize, usize)> = (0..layout.line_count())
        .map(|index| {
            let line = layout.line_metrics(index).unwrap();
            (line.start, line.end, line.newline_len)
        })
        .collect();
    assert_eq!(lines, vec![(0, 3, 2), (3, 5, 1), (5, 6, 0)]);
    assert_eq!(layout.line_metrics(0).unwrap().content_end(), 1);
    let second = layout.line_metrics(1).unwrap();
    assert!((layout.caret_rect(3, false).y - second.top).abs() < 0.01);
    let hit = layout.hit_test_point(Point::new(400.0, second.top + 2.0));
    assert_eq!(
        hit.insertion_offset(),
        4,
        "right of a broken line lands before the break"
    );
}

#[test]
fn style_ranges_affect_the_layout_they_cover() {
    let (_com, engine) = engine();
    let text = "plain bold plain";
    let plain = engine.layout(text, &segoe(16.0), &[], &[], f32::INFINITY);
    let bold = StyledRange {
        range: 6..10,
        style: segoe(16.0).with_weight(FontWeight::BOLD),
    };
    let heavy = engine.layout(
        text,
        &segoe(16.0),
        std::slice::from_ref(&bold),
        &[],
        f32::INFINITY,
    );
    assert!(heavy.size().width > plain.size().width, "bold is wider");
    // Text before the range is unaffected; after it everything shifts right.
    assert!((heavy.caret_rect(6, false).x - plain.caret_rect(6, false).x).abs() < 0.5);
    assert!(heavy.caret_rect(11, false).x > plain.caret_rect(11, false).x);
    // A later overlapping range wins.
    let narrower = StyledRange {
        range: 6..10,
        style: segoe(16.0),
    };
    let overridden = engine.layout(text, &segoe(16.0), &[bold, narrower], &[], f32::INFINITY);
    assert!((overridden.size().width - plain.size().width).abs() < 0.5);
    // A bigger run grows the line.
    let large = StyledRange {
        range: 0..5,
        style: segoe(48.0),
    };
    let tall = engine.layout(text, &segoe(16.0), &[large], &[], f32::INFINITY);
    assert!(tall.size().height > plain.size().height + 15.0);
}

#[test]
fn inline_objects_take_their_size_and_move_the_text() {
    let (_com, engine) = engine();
    let text = "a\u{FFFC}b";
    let plain = engine.layout(text, &segoe(16.0), &[], &[], f32::INFINITY);
    let object = InlineObject {
        at: 1,
        size: Size::new(60.0, 24.0),
        baseline: 18.0,
        id: 42,
    };
    let layout = engine.layout(text, &segoe(16.0), &[], &[object], f32::INFINITY);
    let rect = layout.inline_object_rect(42).unwrap();
    assert_eq!((rect.width, rect.height), (60.0, 24.0));
    assert!((rect.x - layout.caret_rect(1, false).x).abs() < 0.01);
    assert!(
        (layout.caret_rect(4, false).x - (rect.x + 60.0)).abs() < 0.5,
        "the next cluster starts after the object"
    );
    assert!(layout.size().width > plain.size().width + 40.0);
    let line = layout.line_metrics(0).unwrap();
    assert!(
        (rect.y - (line.baseline - 18.0)).abs() < 0.01,
        "the object sits on the baseline"
    );
    assert!(layout.inline_object_rect(43).is_none());
    assert!(plain.inline_object_rect(42).is_none());
}

#[test]
fn engines_can_be_built_with_their_own_font_bytes() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let none = DWriteEngine::with_fonts(Vec::new()).unwrap();
    assert_eq!(none.bundled_font_count(), 0);
    assert!(none.has_family("Segoe UI"));
    assert_eq!(none.resolve_family("Segoe UI"), "Segoe UI");
    let format = none.text_format("Segoe UI", 15.0, 400, true).unwrap();
    // SAFETY: plain getters on a live text format.
    let size = unsafe { format.GetFontSize() };
    assert_eq!(size, 15.0);
}

#[test]
fn invalid_styles_fall_back_to_a_layout_that_keeps_the_text() {
    let (_com, engine) = engine();
    let rejected = engine.layout("keep me", &segoe(-3.0), &[], &[], 100.0);
    assert_eq!(rejected.text(), "keep me");
    assert!(rejected.as_any().downcast_ref::<FailedLayout>().is_some());
    assert_eq!(rejected.size(), Size::ZERO);
    let accepted = engine.layout("keep me", &segoe(12.0), &[], &[], 100.0);
    assert!(accepted.as_any().downcast_ref::<DWriteLayout>().is_some());
    assert!(accepted.size().width > 20.0);
}
