//! Plain-text documents without word wrap (spec §4.1): one row per line, sideways scrolling
//! with the wheel, caret and accessibility, and switching the wrapping while a document is open;
//! and their syntax colours: a plain document with a language colours the tokens of every line,
//! in the calm palette, until the setting is turned off.

mod editor_common;

use std::cell::RefCell;

use editor_common::{COLUMN_LEFT, Harness, close};
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_ui::access::AccessAction;
use ssw_ui::canvas::DrawOp;
use ssw_ui::editor::DocumentKind;
use ssw_ui::editor::highlight::{Language, SyntaxColors, TokenKind};
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::input::{ImeEvent, Modifiers, vk};
use ssw_ui::text::{
    InlineObject, MonoTextEngine, StyledRange, TextLayout, TextLayoutEngine, TextStyle,
};
use ssw_ui::widget::{LayoutCx, Widget};
use ssw_ui::widgets::Control;

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

// ----- syntax colours ------------------------------------------------------------------------

/// A [`MonoTextEngine`] that remembers the text and the style ranges of every layout it makes
/// (the engine of the harness ignores the ranges).
struct Spy {
    inner: MonoTextEngine,
    layouts: RefCell<Vec<(String, Vec<StyledRange>)>>,
}

impl Spy {
    fn new() -> Self {
        Self {
            inner: MonoTextEngine::default(),
            layouts: RefCell::new(Vec::new()),
        }
    }

    fn forget(&self) {
        self.layouts.borrow_mut().clear();
    }
}

impl TextLayoutEngine for Spy {
    fn layout(
        &self,
        text: &str,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Box<dyn TextLayout> {
        self.layouts
            .borrow_mut()
            .push((text.to_owned(), ranges.to_vec()));
        self.inner.layout(text, base, ranges, objects, max_width)
    }
}

/// A layout pass of `h` that measures through `spy`. Blocks are measured again only after
/// something that changes them (the document, the configuration), so call it after such a change.
fn arrange_with(h: &mut Harness, spy: &Spy) {
    let id = h.view.id();
    h.focus.begin_tab_order();
    {
        let mut cx = LayoutCx::new(id, &h.theme, 1.0, spy, &mut h.focus, &mut h.requests);
        let size = h.view.measure(&mut cx, h.size);
        h.view
            .arrange(&mut cx, Rect::from_origin_size(Point::ZERO, size));
    }
    h.focus.commit_tab_order();
}

/// The text and color of every styled stretch of the last layout of the line `line`.
fn stretches(spy: &Spy, line: &str) -> Vec<(String, Option<Color>)> {
    let layouts = spy.layouts.borrow();
    let (_, ranges) = layouts
        .iter()
        .rev()
        .find(|(text, _)| text == line)
        .unwrap_or_else(|| panic!("{line:?} was laid out"));
    ranges
        .iter()
        .map(|styled| (line[styled.range.clone()].to_owned(), styled.style.color))
        .collect()
}

/// A plain document of `text` coloured as Rust, laid out through `spy`.
fn coloured(text: &str, spy: &Spy) -> Harness {
    let mut h = plain(text, true);
    h.config(|config| config.plain_language = Some(Language::Rust));
    arrange_with(&mut h, spy);
    h
}

const CODE: &str = "let name = \"ssw\";\nplain words\n";

#[test]
fn a_plain_document_with_a_language_colours_the_tokens_of_its_lines() {
    let spy = Spy::new();
    let h = coloured(CODE, &spy);
    let calm = SyntaxColors::calm(&h.theme);
    assert_eq!(
        stretches(&spy, "let name = \"ssw\";"),
        [
            ("let".to_owned(), Some(calm.color(TokenKind::Keyword))),
            ("\"ssw\"".to_owned(), Some(calm.color(TokenKind::String)))
        ]
    );
    assert!(stretches(&spy, "plain words").is_empty());
}

#[test]
fn without_a_language_or_a_plain_document_nothing_is_coloured() {
    // No language for the file type.
    let spy = Spy::new();
    let mut h = plain(CODE, true);
    arrange_with(&mut h, &spy);
    assert!(stretches(&spy, "let name = \"ssw\";").is_empty());

    // A language, but the document is shown as Markdown: its own styles stay, the syntax
    // colors of the calm palette never appear.
    let spy = Spy::new();
    let mut h = Harness::new("let name = \"ssw\";\n");
    h.config(|config| config.plain_language = Some(Language::Rust));
    arrange_with(&mut h, &spy);
    let calm = SyntaxColors::calm(&h.theme);
    let syntax = [
        calm.color(TokenKind::Keyword),
        calm.color(TokenKind::String),
        calm.color(TokenKind::Comment),
    ];
    assert!(
        stretches(&spy, "let name = \"ssw\";")
            .iter()
            .all(|(_, color)| color.is_none_or(|color| !syntax.contains(&color)))
    );
}

#[test]
fn turning_syntax_highlighting_off_removes_the_colours_and_on_brings_them_back() {
    let spy = Spy::new();
    let mut h = coloured(CODE, &spy);
    let line = "let name = \"ssw\";";
    assert_eq!(stretches(&spy, line).len(), 2);

    h.config(|config| config.syntax_highlighting = false);
    spy.forget();
    arrange_with(&mut h, &spy);
    assert!(stretches(&spy, line).is_empty());

    h.config(|config| config.syntax_highlighting = true);
    spy.forget();
    arrange_with(&mut h, &spy);
    assert_eq!(stretches(&spy, line).len(), 2);
}

#[test]
fn the_whole_document_is_tokenized_so_a_comment_runs_over_the_lines_after_it() {
    let spy = Spy::new();
    let mut h = coloured("let a = 1;\nlet b = 2;\n", &spy);
    let calm = SyntaxColors::calm(&h.theme);
    assert_eq!(
        stretches(&spy, "let b = 2;"),
        [
            ("let".to_owned(), Some(calm.color(TokenKind::Keyword))),
            ("2".to_owned(), Some(calm.color(TokenKind::Number)))
        ]
    );

    // Typing the start of a block comment on the first line recolours the second one too.
    h.doc
        .borrow_mut()
        .apply(Transaction::single(
            Edit::insert(0, "/* "),
            Selection::caret(0),
            Selection::caret(3),
        ))
        .unwrap();
    spy.forget();
    arrange_with(&mut h, &spy);
    let comment = Some(calm.color(TokenKind::Comment));
    for line in ["/* let a = 1;", "let b = 2;"] {
        let found = stretches(&spy, line);
        assert!(
            !found.is_empty() && found.iter().all(|(_, color)| *color == comment),
            "{line}: {found:?}"
        );
        let covered: String = found.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(covered, line);
    }
}

#[test]
fn a_line_deep_in_a_large_document_gets_the_tokens_of_its_own_text() {
    let text: String = (0..5000).map(|n| format!("let n{n} = {n};\n")).collect();
    let spy = Spy::new();
    let mut h = coloured(&text, &spy);
    let start = h.doc.borrow().buffer().line_range(4000).start;
    h.view.go_to(Selection::caret(start));
    arrange_with(&mut h, &spy);
    let found = stretches(&spy, "let n4000 = 4000;");
    let words: Vec<&str> = found.iter().map(|(text, _)| text.as_str()).collect();
    assert_eq!(words, ["let", "4000"]);
}

#[test]
fn an_ime_composition_inside_a_token_splits_it_and_stays_uncoloured() {
    let spy = Spy::new();
    let mut h = coloured("let s = \"abc\";\n", &spy);
    let calm = SyntaxColors::calm(&h.theme);
    // Between `b` and `c` of the string.
    h.set_caret(11);
    h.ime(ImeEvent::Start);
    h.ime(ImeEvent::Update {
        text: "かな".into(),
        caret: 6,
        attrs: Vec::new(),
    });
    // The composition is not in the document; measure the line again with it displayed.
    h.config(|config| config.wrap = false);
    spy.forget();
    arrange_with(&mut h, &spy);
    let string = Some(calm.color(TokenKind::String));
    assert_eq!(
        stretches(&spy, "let s = \"abかなc\";"),
        [
            ("let".to_owned(), Some(calm.color(TokenKind::Keyword))),
            ("\"ab".to_owned(), string),
            ("c\"".to_owned(), string)
        ]
    );
    assert_eq!(
        h.text(),
        "let s = \"abc\";\n",
        "nothing is written before the commit"
    );
    h.ime(ImeEvent::End);
}
