//! Live rendering of the editor: marker hiding and reveal, caret and selection geometry, hit
//! testing through the display map, IME composition, inline objects, focus mode, folding hooks
//! and the accessibility node.

mod editor_common;

use std::rc::Rc;

use editor_common::{ADVANCE, Harness, LINE, TOP, close};
use ssw_core::edit::Selection;
use ssw_core::text::TextRange;
use ssw_ui::access::Role;
use ssw_ui::canvas::{DrawOp, SceneId};
use ssw_ui::editor::object::{
    InlineObjectProvider, ObjectInfo, ObjectKind, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::editor::{EditorEvent, LinkTarget};
use ssw_ui::geometry::{Point, Size};
use ssw_ui::input::{
    Event, FocusEvent, ImeAttr, ImeAttrStyle, ImeEvent, Modifiers, PointerEventKind,
};

#[test]
fn inline_markers_hide_until_the_caret_touches_the_element() {
    let mut h = Harness::new("a **bold** b\n");
    h.set_caret(13);
    assert_eq!(h.painted_texts(), ["a bold b"]);
    h.set_caret(4);
    assert_eq!(h.painted_texts(), ["a **bold** b"], "inside the element");
    h.set_caret(2);
    assert_eq!(
        h.painted_texts(),
        ["a **bold** b"],
        "the opening edge touches"
    );
    h.set_caret(10);
    assert_eq!(
        h.painted_texts(),
        ["a **bold** b"],
        "the closing edge touches"
    );
    h.set_caret(11);
    assert_eq!(h.painted_texts(), ["a bold b"], "past the element");
}

#[test]
fn a_selection_reveals_every_element_it_touches() {
    let mut h = Harness::new("*a* and `b` and ==c==\n");
    h.set_caret(22);
    assert_eq!(h.painted_texts(), ["a and b and c"]);
    h.view.set_selection(Selection::new(0, 9));
    assert_eq!(h.painted_texts(), ["*a* and `b` and c"]);
}

#[test]
fn turning_auto_hide_off_shows_every_marker() {
    let mut h = Harness::new("a **bold** b\n");
    h.set_caret(13);
    h.config(|config| config.auto_hide_markdown = false);
    assert_eq!(h.painted_texts(), ["a **bold** b"]);
}

#[test]
fn heading_markers_and_badge_show_only_with_the_caret_on_the_heading() {
    let mut h = Harness::new("# Title\n\nbody\n");
    h.set_caret(0);
    let texts = h.painted_texts();
    assert!(texts.contains(&"# Title".to_owned()), "{texts:?}");
    assert!(
        texts.contains(&"H1".to_owned()),
        "the gutter badge: {texts:?}"
    );
    h.set_caret(10);
    let texts = h.painted_texts();
    assert!(texts.contains(&"Title".to_owned()), "{texts:?}");
    assert!(!texts.contains(&"H1".to_owned()));
}

#[test]
fn headings_are_laid_out_at_their_scale() {
    let h = Harness::new("# Big\n");
    let canvas = h.paint();
    let text = canvas
        .recorded()
        .iter()
        .find(|entry| matches!(&entry.op, DrawOp::Text { text, .. } if text == "# Big"))
        .and_then(|entry| entry.bounds)
        .expect("heading text");
    // Body 16 × 1.75 = 28 DIP font, mono line 28 × 1.2 = 33.6, padded above by 0.35 em.
    assert!(close(text.height, 33.6), "{text:?}");
    assert!(close(text.y, TOP + 28.0 * 0.35), "{text:?}");
}

#[test]
fn the_caret_is_drawn_where_the_source_offset_maps_to() {
    let mut h = Harness::new("abc\n");
    h.set_caret(2);
    let canvas = h.paint();
    let rect = h.view.caret_rect().expect("caret in view");
    assert!(close(rect.x, h.left + 2.0 * ADVANCE), "{rect:?}");
    assert!(close(rect.y, TOP), "{rect:?}");
    assert!(close(rect.height, LINE), "{rect:?}");
    let caret_color = h.theme.tokens.caret;
    let drawn = canvas
        .recorded()
        .iter()
        .find(|entry| matches!(&entry.op, DrawOp::FillRect { color, .. } if *color == caret_color))
        .and_then(|entry| entry.bounds)
        .expect("caret fill");
    assert!(
        close(drawn.x, rect.x) && close(drawn.width, 1.5),
        "{drawn:?}"
    );
    // Hidden markers do not shift the caret: `**ab**` shows `ab`, the caret after `a`.
    let mut h = Harness::new("**ab**\n");
    h.set_caret(7);
    h.paint();
    h.set_caret(3);
    h.paint();
    assert_eq!(
        h.painted_texts(),
        ["**ab**"],
        "touched: markers are visible"
    );
    let rect = h.view.caret_rect().expect("caret");
    assert!(close(rect.x, h.left + 3.0 * ADVANCE), "{rect:?}");
}

#[test]
fn the_caret_blinks_on_the_timer_and_stops_drawing_when_unfocused() {
    let mut h = Harness::new("abc\n");
    let caret_color = h.theme.tokens.caret;
    let carets = |h: &Harness| {
        h.paint()
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == caret_color))
            .count()
    };
    assert_eq!(carets(&h), 1);
    let timer = h
        .requests
        .timers()
        .iter()
        .find_map(|request| match request {
            ssw_ui::widget::TimerRequest::Set { id, .. } => Some(*id),
            ssw_ui::widget::TimerRequest::Kill(_) => None,
        })
        .expect("the view started its blink timer on focus");
    h.event(Event::Timer(timer));
    assert_eq!(carets(&h), 0, "blinked off");
    h.event(Event::Timer(timer));
    assert_eq!(carets(&h), 1, "blinked on");
    h.press(0x27);
    h.event(Event::Timer(timer));
    h.press(0x27);
    assert_eq!(carets(&h), 1, "typing and navigation show the caret");
    h.event(Event::Focus(FocusEvent::Lost));
    assert_eq!(carets(&h), 0, "no caret without focus");
}

#[test]
fn selections_paint_in_the_active_and_inactive_colors() {
    let mut h = Harness::new("hello world\n");
    h.view.set_selection(Selection::new(0, 5));
    let active = h.theme.tokens.selection_bg;
    let canvas = h.paint();
    let rects: Vec<_> = canvas
        .recorded()
        .iter()
        .filter(|entry| matches!(&entry.op, DrawOp::FillRect { color, .. } if *color == active))
        .filter_map(|entry| entry.bounds)
        .collect();
    assert_eq!(rects.len(), 1);
    assert!(close(rects[0].x, h.left) && close(rects[0].width, 5.0 * ADVANCE));
    assert!(close(rects[0].y, TOP) && close(rects[0].height, LINE));
    h.event(Event::Focus(FocusEvent::Lost));
    let inactive = h.theme.tokens.selection_inactive_bg;
    let canvas = h.paint();
    assert!(
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == inactive))
    );
}

#[test]
fn multi_block_selections_paint_every_line() {
    let mut h = Harness::new("one\n\ntwo\n");
    h.view.set_selection(Selection::new(1, 7));
    let active = h.theme.tokens.selection_bg;
    let rects: Vec<_> = h
        .paint()
        .recorded()
        .iter()
        .filter(|entry| matches!(&entry.op, DrawOp::FillRect { color, .. } if *color == active))
        .filter_map(|entry| entry.bounds)
        .collect();
    // `ne` in the first block, the blank line between, `tw` in the second.
    assert_eq!(rects.len(), 3, "{rects:?}");
    assert!(rects[0].y < rects[1].y && rects[1].y < rects[2].y);
}

#[test]
fn clicks_map_through_hidden_markers_and_then_reveal_them() {
    let mut h = Harness::new("**ab**\n");
    h.set_caret(7);
    h.paint();
    assert_eq!(h.painted_texts(), ["ab"]);
    h.click(h.first_line_point(1), 1, Modifiers::NONE);
    assert_eq!(h.selection(), Selection::caret(3), "between a and b");
    assert_eq!(h.painted_texts(), ["**ab**"]);
    let rect = h.view.caret_rect().expect("caret");
    assert!(close(rect.x, h.left + 3.0 * ADVANCE), "{rect:?}");
}

#[test]
fn double_and_triple_clicks_select_a_word_and_a_paragraph() {
    let mut h = Harness::new("one two three\nfour\n\nnext\n");
    h.click(h.first_line_point(5), 2, Modifiers::NONE);
    assert_eq!(h.selection(), Selection::new(4, 7), "two");
    h.click(h.first_line_point(5), 3, Modifiers::NONE);
    assert_eq!(h.selection(), Selection::new(0, 18), "the whole paragraph");
    h.click(h.first_line_point(1), 1, Modifiers::NONE);
    h.click(h.first_line_point(6), 1, Modifiers::SHIFT);
    assert_eq!(h.selection(), Selection::new(1, 6), "shift+click extends");
}

#[test]
fn dragging_selects_and_releases_the_pointer() {
    let mut h = Harness::new("abcdefgh\n");
    let start = h.first_line_point(1);
    h.pointer(PointerEventKind::Down, start, 1, Modifiers::NONE);
    assert!(
        h.capture.owner().is_some(),
        "the press captures the pointer"
    );
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(5),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::new(1, 5));
    // Dragging back past the anchor reverses the selection.
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(0),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::new(1, 0));
    h.pointer(
        PointerEventKind::Up,
        h.first_line_point(0),
        0,
        Modifiers::NONE,
    );
    assert!(h.capture.owner().is_none());
    assert!(
        h.events()
            .iter()
            .any(|event| matches!(event, EditorEvent::SelectionToolbar { .. })),
        "a mouse selection offers the toolbar"
    );
}

#[test]
fn ctrl_click_on_a_link_reports_its_target() {
    let mut h = Harness::new("[t](http://x)\n");
    h.set_caret(14);
    h.paint();
    assert_eq!(h.painted_texts(), ["t"]);
    h.click(h.first_line_point(0), 1, Modifiers::CTRL);
    assert_eq!(
        h.events()
            .into_iter()
            .filter(|event| matches!(event, EditorEvent::LinkActivated(_)))
            .collect::<Vec<_>>(),
        [EditorEvent::LinkActivated(LinkTarget::Url(
            "http://x".into()
        ))]
    );
    assert_eq!(
        h.selection(),
        Selection::caret(14),
        "the caret did not move"
    );
}

#[test]
fn clicking_a_footnote_reference_jumps_to_its_definition() {
    let mut h = Harness::new("a[^1]\n\n[^1]: note\n");
    h.set_caret(18);
    h.paint();
    assert_eq!(h.painted_texts()[0], "a1");
    h.click(h.first_line_point(1), 1, Modifiers::NONE);
    assert_eq!(h.selection(), Selection::caret(7), "the definition block");
}

#[test]
fn ime_composition_is_displayed_with_an_underline_and_not_written() {
    let mut h = Harness::new("ab\n");
    h.set_caret(2);
    h.ime(ImeEvent::Start);
    h.ime(ImeEvent::Update {
        text: "かな".into(),
        caret: 3,
        attrs: vec![
            ImeAttr {
                range: 0..3,
                style: ImeAttrStyle::TargetConverted,
            },
            ImeAttr {
                range: 3..6,
                style: ImeAttrStyle::Input,
            },
        ],
    });
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["abかな"]);
    assert_eq!(h.text(), "ab\n", "nothing is written before the commit");
    let widths: Vec<f32> = canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::StrokeLine { width, .. } => Some(*width),
            _ => None,
        })
        .collect();
    assert_eq!(widths, [2.0, 1.0], "the converted target is thicker");
    let caret = h.view.caret_rect().expect("caret");
    assert!(
        close(caret.x, h.left + 3.0 * ADVANCE),
        "inside the composition: {caret:?}"
    );
    h.ime(ImeEvent::Commit("か".into()));
    h.ime(ImeEvent::End);
    assert_eq!(h.text(), "abか\n");
    assert_eq!(h.painted_texts(), ["abか"]);
    assert_eq!(h.selection(), Selection::caret(5));
}

#[test]
fn composition_replaces_a_selection_and_end_cancels() {
    let mut h = Harness::new("abcd\n");
    h.view.set_selection(Selection::new(1, 3));
    h.ime(ImeEvent::Start);
    h.ime(ImeEvent::Update {
        text: "x".into(),
        caret: 1,
        attrs: vec![],
    });
    assert_eq!(
        h.text(),
        "ad\n",
        "the selected text is replaced by the composition"
    );
    assert_eq!(h.painted_texts(), ["axd"]);
    h.ime(ImeEvent::End);
    assert_eq!(h.painted_texts(), ["ad"]);
}

struct MathProvider;

impl InlineObjectProvider for MathProvider {
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState {
        match (request.kind, request.source) {
            (ObjectKind::Math, "x^2") => ObjectState::Ready(ObjectInfo {
                visual: ObjectVisual::Scene(SceneId(5)),
                size: Size::new(30.0, 20.0),
                baseline: 15.0,
            }),
            (ObjectKind::Math, _) => ObjectState::Failed,
            (ObjectKind::Image, _) => ObjectState::Pending,
        }
    }
}

#[test]
fn inline_math_becomes_a_scene_at_the_placeholder() {
    let mut h = Harness::new("a $x^2$ b\n");
    h.view.set_object_provider(Rc::new(MathProvider));
    h.set_caret(10);
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["a \u{FFFC} b"]);
    let scene = canvas
        .recorded()
        .iter()
        .find_map(|entry| match (&entry.op, entry.bounds) {
            (DrawOp::Scene { scene, dest }, _) => Some((*scene, *dest)),
            _ => None,
        })
        .expect("a scene was drawn");
    assert_eq!(scene.0, SceneId(5));
    assert!(close(scene.1.width, 30.0) && close(scene.1.height, 20.0));
    // Touching the element shows the TeX source instead.
    h.set_caret(4);
    assert_eq!(h.painted_texts(), ["a $x^2$ b"]);
}

#[test]
fn failed_and_pending_objects_show_their_source() {
    let mut h = Harness::new("$bad$ ![i](p.png)\n");
    h.view.set_object_provider(Rc::new(MathProvider));
    h.set_caret(18);
    assert_eq!(h.painted_texts(), ["$bad$ ![i](p.png)"]);
}

#[test]
fn focus_mode_dims_every_block_but_the_current_one() {
    let mut h = Harness::new("one\n\ntwo\n\nthree\n");
    h.view.set_focus_mode(true);
    h.set_caret(5);
    let canvas = h.paint();
    let layers: Vec<f32> = canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::PushLayer { opacity } => Some(*opacity),
            _ => None,
        })
        .collect();
    assert_eq!(layers.len(), 2);
    assert!(layers.iter().all(|opacity| *opacity < 0.5));
    h.view.set_focus_mode(false);
    assert!(
        h.paint()
            .ops()
            .all(|op| !matches!(op, DrawOp::PushLayer { .. }))
    );
}

#[test]
fn folded_sections_are_not_laid_out_and_the_heading_shows_a_pill() {
    let mut h = Harness::new("# H\n\nbody\n\nnext\n");
    h.set_caret(11);
    h.view.set_folds(vec![TextRange::new(3, 9)], vec![0]);
    assert_eq!(h.painted_texts(), ["H", "…", "next"]);
    assert!(h.view.is_offset_hidden(6));
    assert!(
        !h.view.is_offset_hidden(3),
        "the heading itself stays visible"
    );
    // The chevron of the heading toggles the fold.
    let chevron = Point::new(h.left - 10.0, TOP + 26.0);
    h.click(chevron, 1, Modifiers::NONE);
    assert_eq!(
        h.events()
            .into_iter()
            .filter(|event| matches!(event, EditorEvent::ToggleFold { .. }))
            .collect::<Vec<_>>(),
        [EditorEvent::ToggleFold { heading_start: 0 }]
    );
    h.view.set_folds(Vec::new(), Vec::new());
    assert!(h.painted_texts().contains(&"body".to_owned()));
}

#[test]
fn the_heading_badge_opens_the_level_menu() {
    let mut h = Harness::sized("## Two\n", Size::new(1000.0, 400.0));
    h.set_caret(3);
    h.paint();
    let badge = Point::new(h.left - 40.0, TOP + 26.0);
    h.click(badge, 1, Modifiers::NONE);
    let events = h.events();
    let menu = events
        .iter()
        .find_map(|event| match event {
            EditorEvent::HeadingMenu { level, range, .. } => Some((*level, *range)),
            _ => None,
        })
        .expect("a menu request");
    assert_eq!(menu, (2, TextRange::new(0, 6)));
}

#[test]
fn the_access_node_describes_a_document_with_text_pattern_data() {
    let mut h = Harness::new("# Title\n\nbody\n");
    h.view.set_selection(Selection::new(2, 5));
    let node = h.access();
    assert_eq!(node.role, Role::Document);
    assert_eq!(node.name, "Editor");
    assert_eq!(node.description, "Heading level 1");
    assert!(node.state.focused && node.state.multiline);
    let text = node.patterns.text.expect("text pattern");
    assert_eq!(text.len, 14);
    assert_eq!(text.selection, [2..5]);
    assert_eq!(text.caret, 5);
    assert_eq!(text.visible, 0..14);
    assert!(
        !node
            .patterns
            .scroll
            .expect("scroll pattern")
            .vertically_scrollable
    );
}
