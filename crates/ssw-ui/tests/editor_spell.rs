//! Spell check in the editor (spec §2.6): squiggles under misspelled words, what is skipped,
//! caching, laziness, the context-menu event and the menu actions.

mod editor_common;

use std::cell::RefCell;
use std::collections::HashSet;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use editor_common::{ADVANCE, COLUMN_LEFT, Harness, TOP, close};
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::text::TextRange;
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::editor::EditorEvent;
use ssw_ui::editor::block::{BlockClass, ParagraphRenderer};
use ssw_ui::editor::spell::{self, SpellCheck, SpellProvider, SpellRenderer, SpellStrings};
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Cursor, Event, Modifiers, PointerButton, PointerEvent, PointerEventKind, vk};

/// The words of `text` (letters, digits and apostrophes) as byte ranges.
fn words(text: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut start = None;
    for (index, c) in text.char_indices() {
        let inside = c.is_alphanumeric() || c == '\'';
        match (inside, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                found.push(from..index);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        found.push(from..text.len());
    }
    found
}

/// A spelling checker over a word list that records what it was asked.
#[derive(Default)]
struct Dictionary {
    known: RefCell<HashSet<String>>,
    checked: RefCell<Vec<String>>,
    added: RefCell<Vec<String>>,
    ignored: RefCell<Vec<String>>,
}

impl Dictionary {
    fn new(known: &[&str]) -> Rc<Self> {
        let dictionary = Self::default();
        for word in known {
            dictionary.known.borrow_mut().insert((*word).to_owned());
        }
        Rc::new(dictionary)
    }

    fn checked(&self) -> Vec<String> {
        self.checked.borrow().clone()
    }
}

impl SpellProvider for Dictionary {
    fn check(&self, text: &str) -> Vec<Range<usize>> {
        self.checked.borrow_mut().push(text.to_owned());
        words(text)
            .into_iter()
            .filter(|range| {
                !self
                    .known
                    .borrow()
                    .contains(&text[range.clone()].to_lowercase())
            })
            .collect()
    }

    fn suggestions(&self, word: &str) -> Vec<String> {
        match word {
            "quikc" => vec!["quick".to_owned(), "quirk".to_owned()],
            _ => Vec::new(),
        }
    }

    fn add_to_dictionary(&self, word: &str) {
        self.known.borrow_mut().insert(word.to_lowercase());
        self.added.borrow_mut().push(word.to_owned());
    }

    fn ignore(&self, word: &str) {
        self.known.borrow_mut().insert(word.to_lowercase());
        self.ignored.borrow_mut().push(word.to_owned());
    }
}

const SENTENCE: &str = "The quikc brown fox\n";
const SENTENCE_WORDS: [&str; 5] = ["the", "brown", "fox", "quick", "word"];

/// A budget that no test's queue outlasts.
const LONG: Duration = Duration::from_secs(60);

/// Paints, lets the service answer what the painted blocks asked for and paints again: the
/// frame the host draws after its idle turn.
fn settled(h: &Harness, spell: &SpellCheck) -> RecordingCanvas {
    h.paint();
    spell.pump(LONG);
    h.paint()
}

/// An editor with the service registered, painted once (the blocks asked for their checks) and
/// answered; the next paint shows the squiggles.
fn editor_with(harness: Harness, known: &[&str]) -> (Harness, Rc<Dictionary>, Rc<SpellCheck>) {
    let mut h = harness;
    let provider = Dictionary::new(known);
    let spell = Rc::new(SpellCheck::new(provider.clone()));
    spell::register(h.view.registry_mut(), Rc::clone(&spell));
    h.paint();
    spell.pump(LONG);
    (h, provider, spell)
}

fn editor(text: &str, known: &[&str]) -> (Harness, Rc<Dictionary>, Rc<SpellCheck>) {
    editor_with(Harness::new(text), known)
}

/// The areas of the squiggles in `canvas` (strokes in the error color).
fn squiggles(h: &Harness, canvas: &RecordingCanvas) -> Vec<Rect> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match &entry.op {
            DrawOp::StrokePath { color, .. } if *color == h.theme.tokens.error => entry.bounds,
            _ => None,
        })
        .collect()
}

/// The first drawn text and where it is.
fn first_text(canvas: &RecordingCanvas) -> Rect {
    canvas
        .recorded()
        .iter()
        .find_map(|entry| match &entry.op {
            DrawOp::Text { .. } => entry.bounds,
            _ => None,
        })
        .expect("a text is drawn")
}

fn right_click(h: &mut Harness, position: Point) {
    h.event(Event::Pointer(PointerEvent::mouse(
        PointerEventKind::Down,
        Some(PointerButton::Right),
        position,
    )));
}

// ----- squiggles ------------------------------------------------------------------------------

#[test]
fn a_misspelled_word_gets_a_wavy_underline_along_the_bottom_of_its_line() {
    let (h, provider, _) = editor(SENTENCE, &SENTENCE_WORDS);
    let canvas = h.paint();
    let squiggles = squiggles(&h, &canvas);
    assert_eq!(squiggles.len(), 1, "only quikc is misspelled");
    let line = first_text(&canvas);
    // The word starts at column 4 and is 5 characters wide; the stroke adds half its width.
    let area = squiggles[0];
    assert!(close(area.x, line.x + 4.0 * ADVANCE - 0.5), "{area:?}");
    assert!(close(area.width, 5.0 * ADVANCE + 1.0), "{area:?}");
    // The wave lies inside the last 4 DIPs of the line.
    assert!(
        area.y >= line.bottom() - 4.0 && area.bottom() <= line.bottom(),
        "{area:?}"
    );
    let stroke = canvas.ops().find_map(|op| match op {
        DrawOp::StrokePath {
            path, width, dash, ..
        } => Some((path.verbs().len(), *width, *dash)),
        _ => None,
    });
    let (verbs, width, dash) = stroke.expect("a path");
    assert!(verbs > 10, "a zigzag, not a straight line: {verbs}");
    assert!(close(width, 1.0));
    assert_eq!(dash, ssw_ui::canvas::Dash::Solid);
    assert_eq!(provider.checked(), [SENTENCE.trim_end()]);
}

#[test]
fn a_word_split_by_formatting_markers_is_checked_whole_and_underlined_whole() {
    let (h, provider, _) = editor("A mis**spel**ed word\n", &SENTENCE_WORDS_WITH_A);
    let canvas = h.paint();
    assert_eq!(provider.checked(), ["A misspeled word"]);
    let squiggles = squiggles(&h, &canvas);
    assert_eq!(squiggles.len(), 1);
    let line = first_text(&canvas);
    // The markers are hidden, so the word is `misspeled`, 9 characters starting at column 2.
    assert!(
        close(squiggles[0].x, line.x + 2.0 * ADVANCE - 0.5),
        "{:?}",
        squiggles[0]
    );
    assert!(
        close(squiggles[0].width, 9.0 * ADVANCE + 1.0),
        "{:?}",
        squiggles[0]
    );
}

const SENTENCE_WORDS_WITH_A: [&str; 2] = ["a", "word"];

#[test]
fn code_math_urls_link_targets_html_and_front_matter_are_not_checked() {
    let text = "---\ntitle: zzfront\n---\n\n\
        Word `zzcode` and $zzmath$ plus https://zzurl.example then [[zztarget]] and \
        [[page|zzalias]] then [lnk](http://zzdest.example) ![zzalt](zzimg.png) end<br>[^1]\n\n\
        ```\nzzfence\n```\n\n$$\nzzblock\n$$\n\n    zzindented\n\n<div>zzhtml</div>\n\n\
        [^1]: zzfoot\n";
    let known = ["word", "and", "plus", "then", "lnk", "end"];
    let (h, provider, _) = editor(text, &known);
    let canvas = h.paint();
    let checked = provider.checked();
    assert_eq!(checked.len(), 1, "one paragraph: {checked:?}");
    let seen: Vec<&str> = words(&checked[0])
        .into_iter()
        .map(|range| &checked[0][range])
        .collect();
    assert_eq!(
        seen,
        [
            "Word", "and", "plus", "then", "and", "zzalias", "then", "lnk", "zzalt", "end"
        ]
    );
    assert_eq!(
        squiggles(&h, &canvas).len(),
        2,
        "the wiki link alias and the image alt text are prose"
    );
}

#[test]
fn a_word_touching_an_entity_or_an_escape_is_not_judged() {
    let (h, provider, _) = editor("caf&eacute; and tst\\*x ok\n", &["and", "ok"]);
    let canvas = h.paint();
    let checked = provider.checked();
    assert_eq!(checked.len(), 1);
    assert!(checked[0].contains("caf") && checked[0].contains("tst"));
    assert!(
        squiggles(&h, &canvas).is_empty(),
        "caf, tst and x all touch an entity or an escape"
    );
}

#[test]
fn headings_are_checked_and_keep_their_fold_chevron() {
    let (mut h, provider, _) = editor("# Hedaing\n\nbody\n", &["body"]);
    let canvas = h.paint();
    assert_eq!(squiggles(&h, &canvas).len(), 1);
    assert_eq!(provider.checked(), ["Hedaing", "body"]);
    h.events();
    h.click(
        Point::new(COLUMN_LEFT - 12.0, TOP + 28.0),
        1,
        Modifiers::NONE,
    );
    assert!(
        h.events()
            .iter()
            .any(|event| matches!(event, EditorEvent::ToggleFold { heading_start: 0 })),
        "the gutter of the wrapped heading still answers"
    );
}

#[test]
fn a_long_word_that_wraps_is_underlined_on_every_line() {
    let word = "z".repeat(24);
    let harness = Harness::sized(&format!("{word}\n"), Size::new(160.0, 200.0));
    let (h, _, _) = editor_with(harness, &[]);
    // 112 DIPs of text column: 14 characters on the first line, 10 on the second.
    let canvas = h.paint();
    let mut areas = squiggles(&h, &canvas);
    areas.sort_by(|a, b| a.y.total_cmp(&b.y));
    assert_eq!(areas.len(), 2);
    assert!(
        close(areas[0].width, 14.0 * ADVANCE + 1.0),
        "{:?}",
        areas[0]
    );
    assert!(
        close(areas[1].width, 10.0 * ADVANCE + 1.0),
        "{:?}",
        areas[1]
    );
    assert!(areas[1].y > areas[0].y);
}

#[test]
fn nothing_the_provider_reports_that_is_not_a_word_of_the_text_is_drawn() {
    struct Bogus;
    impl SpellProvider for Bogus {
        fn check(&self, _text: &str) -> Vec<Range<usize>> {
            // Checked text: "a   b é" — an empty range, the separator made for the code span, a
            // range that starts in the text before the code span and ends in the separator, a
            // range past the end, a reversed range, a range inside the two bytes of é, and é.
            vec![
                3..3,
                2..3,
                1..3,
                100..105,
                Range { start: 5, end: 2 },
                7..8,
                6..8,
            ]
        }
        fn suggestions(&self, _word: &str) -> Vec<String> {
            Vec::new()
        }
        fn add_to_dictionary(&self, _word: &str) {}
        fn ignore(&self, _word: &str) {}
    }
    let mut h = Harness::new("a `x` b é\n");
    let spell = Rc::new(SpellCheck::new(Rc::new(Bogus)));
    spell::register(h.view.registry_mut(), Rc::clone(&spell));
    let canvas = settled(&h, &spell);
    let areas = squiggles(&h, &canvas);
    assert_eq!(areas.len(), 1, "only é is a word of the text");
    let line = first_text(&canvas);
    // The display text is "a x b é" (the code markers are hidden): é is its seventh character.
    assert!(
        close(areas[0].x, line.x + 6.0 * ADVANCE - 0.5),
        "{:?}",
        areas[0]
    );
    assert!(close(areas[0].width, ADVANCE + 1.0), "{:?}", areas[0]);
}

// ----- caching and laziness -------------------------------------------------------------------

#[test]
fn results_are_cached_per_text_across_layouts_and_edits_elsewhere() {
    let (mut h, provider, spell) = editor("teh first\n\nsecond zzx\n", &["first", "second"]);
    assert_eq!(provider.checked().len(), 2);
    for offset in [0, 5, 12, 15] {
        h.set_caret(offset);
        h.paint();
    }
    assert_eq!(provider.checked().len(), 2, "moving the caret asks nothing");
    assert!(!spell.pending(), "and nothing waits");
    h.set_caret(h.text().len() - 1);
    h.type_text("y");
    h.paint();
    assert_eq!(
        provider.checked().len(),
        2,
        "painting never calls the provider"
    );
    assert!(spell.pending(), "the edited paragraph waits for its check");
    spell.pump(LONG);
    assert!(!spell.pending());
    let checked = provider.checked();
    assert_eq!(checked.len(), 3, "only the edited paragraph: {checked:?}");
    assert_eq!(checked[2], "second zzxy");
    assert_eq!(
        checked.iter().filter(|text| text.contains("teh")).count(),
        1,
        "the first paragraph was checked once"
    );
}

#[test]
fn only_the_blocks_near_the_viewport_are_checked_until_the_document_scrolls() {
    let text: String = (0..300).map(|i| format!("para{i}\n\n")).collect();
    let (mut h, provider, spell) = editor(&text, &[]);
    let first = provider.checked().len();
    assert!(
        (1..60).contains(&first),
        "{first} paragraphs checked out of 300"
    );
    let index = |text: &String| {
        text.trim_start_matches("para")
            .parse::<usize>()
            .unwrap_or(0)
    };
    assert!(provider.checked().iter().all(|text| index(text) < 50));
    h.view.set_scroll_y(6000.0);
    h.paint();
    assert_eq!(provider.checked().len(), first, "painting asks nothing");
    spell.pump(LONG);
    assert!(provider.checked().len() > first, "scrolling checks more");
    assert!(provider.checked().iter().any(|text| index(text) >= 50));
}

#[test]
fn the_cache_starts_over_when_it_is_full() {
    let provider = Dictionary::new(&[]);
    let spell = SpellCheck::new(provider.clone());
    for index in 0..2048 {
        assert_eq!(spell.misspellings(&format!("word{index}")).len(), 1);
    }
    assert_eq!(provider.checked().len(), 2048);
    let _ = spell.misspellings("word0");
    assert_eq!(provider.checked().len(), 2048, "still cached");
    let _ = spell.misspellings("one more");
    assert_eq!(provider.checked().len(), 2049);
    let _ = spell.misspellings("word5");
    assert_eq!(
        provider.checked().len(),
        2050,
        "forgotten when the cache was emptied"
    );
    let _ = spell.misspellings("word5");
    assert_eq!(provider.checked().len(), 2050);
    assert!(format!("{spell:?}").contains("cached_texts"));
}

#[test]
fn switching_the_service_off_removes_the_squiggles_without_asking_again_when_it_comes_back() {
    let (mut h, provider, spell) = editor(SENTENCE, &SENTENCE_WORDS);
    assert!(spell.enabled());
    assert_eq!(squiggles(&h, &h.paint()).len(), 1);
    spell.set_enabled(false);
    h.view.invalidate_objects();
    assert!(!spell.enabled());
    assert!(squiggles(&h, &h.paint()).is_empty());
    spell.set_enabled(true);
    h.view.invalidate_objects();
    assert_eq!(squiggles(&h, &h.paint()).len(), 1);
    assert_eq!(provider.checked().len(), 1, "the answer was cached");
    spell.clear_cache();
    h.view.invalidate_objects();
    assert!(squiggles(&h, &h.paint()).is_empty(), "no answer yet");
    assert_eq!(
        provider.checked().len(),
        1,
        "the provider is asked by the pump"
    );
    spell.pump(LONG);
    assert_eq!(provider.checked().len(), 2, "asked again after clearing");
    assert_eq!(squiggles(&h, &h.paint()).len(), 1);
}

// ----- deferred checks ------------------------------------------------------------------------

#[test]
fn a_painted_block_asks_for_its_check_and_shows_the_squiggle_once_the_pump_answered() {
    let mut h = Harness::new(SENTENCE);
    let provider = Dictionary::new(&SENTENCE_WORDS);
    let spell = Rc::new(SpellCheck::new(provider.clone()));
    spell::register(h.view.registry_mut(), Rc::clone(&spell));
    assert!(!spell.pending());
    let canvas = h.paint();
    assert!(
        squiggles(&h, &canvas).is_empty(),
        "laying out and painting never wait for the checker"
    );
    assert!(provider.checked().is_empty());
    assert!(spell.pending());
    assert!(format!("{spell:?}").contains("waiting_texts: 1"));
    assert_eq!(spell.pump(LONG), 1);
    assert!(!spell.pending());
    assert_eq!(squiggles(&h, &h.paint()).len(), 1);
    assert_eq!(spell.pump(LONG), 0, "nothing waits any more");
    h.paint();
    assert!(!spell.pending(), "an answered block does not ask again");
}

#[test]
fn a_pump_checks_at_least_one_text_and_stops_when_its_budget_is_spent() {
    let provider = Dictionary::new(&[]);
    let spell = SpellCheck::new(provider.clone());
    for text in ["one", "two", "three"] {
        spell.request(text);
    }
    assert_eq!(
        spell.pump(Duration::ZERO),
        1,
        "one text per call, oldest first"
    );
    assert_eq!(provider.checked(), ["one"]);
    assert_eq!(spell.pump(Duration::ZERO), 1);
    assert!(spell.pending());
    assert_eq!(spell.pump(LONG), 1);
    assert_eq!(provider.checked(), ["one", "two", "three"]);
    assert_eq!(spell.pump(LONG), 0);
    assert!(!spell.pending());
}

#[test]
fn requests_keep_their_place_and_the_oldest_are_forgotten_when_too_many_wait() {
    let provider = Dictionary::new(&[]);
    let spell = SpellCheck::new(provider.clone());
    spell.request("first");
    spell.request("second");
    spell.request("first");
    assert!(format!("{spell:?}").contains("waiting_texts: 2"));
    for index in 0..64 {
        spell.request(&format!("text{index}"));
    }
    assert!(format!("{spell:?}").contains("waiting_texts: 64"));
    assert_eq!(spell.pump(LONG), 64);
    let checked = provider.checked();
    assert_eq!(checked.len(), 64);
    assert_eq!(checked[0], "text0", "first and second were forgotten");
    assert_eq!(checked[63], "text63");
}

#[test]
fn a_text_answered_meanwhile_is_not_asked_again_and_a_service_that_is_off_forgets_its_requests() {
    let provider = Dictionary::new(&[]);
    let spell = SpellCheck::new(provider.clone());
    assert!(spell.cached("asked twice").is_none());
    spell.request("asked twice");
    let _ = spell.misspellings("asked twice");
    assert!(spell.cached("asked twice").is_some());
    assert_eq!(spell.pump(LONG), 0);
    assert_eq!(provider.checked().len(), 1);

    spell.request("waits");
    assert!(spell.pending());
    spell.set_enabled(false);
    assert!(!spell.pending(), "a service that is off has nothing to do");
    assert_eq!(spell.pump(LONG), 0);
    spell.set_enabled(true);
    assert!(!spell.pending(), "the request was forgotten");
    assert_eq!(provider.checked().len(), 1);
}

#[test]
fn a_block_whose_request_was_lost_asks_again_the_next_time_it_is_painted() {
    let mut h = Harness::new(SENTENCE);
    let provider = Dictionary::new(&SENTENCE_WORDS);
    let spell = Rc::new(SpellCheck::new(provider.clone()));
    spell::register(h.view.registry_mut(), Rc::clone(&spell));
    h.paint();
    assert!(spell.pending());
    // The request goes away (the service was switched off and on again).
    spell.set_enabled(false);
    spell.pump(Duration::ZERO);
    spell.set_enabled(true);
    assert!(!spell.pending());
    h.paint();
    assert!(spell.pending(), "the unanswered block asked once more");
    spell.pump(LONG);
    assert_eq!(squiggles(&h, &h.paint()).len(), 1);
    assert_eq!(provider.checked().len(), 1);
}

#[test]
fn a_text_known_to_be_clean_is_laid_out_without_asking_again() {
    let (mut h, provider, spell) = editor("all words known\n", &["all", "words", "known"]);
    assert_eq!(provider.checked().len(), 1);
    h.view.invalidate_objects();
    assert!(squiggles(&h, &h.paint()).is_empty());
    assert!(!spell.pending());
    assert_eq!(provider.checked().len(), 1);
}

// ----- the context menu -----------------------------------------------------------------------

#[test]
fn a_right_click_on_a_misspelled_word_raises_the_menu_with_suggestions() {
    let (mut h, _, _) = editor(SENTENCE, &SENTENCE_WORDS);
    let line = first_text(&h.paint());
    h.events();
    right_click(
        &mut h,
        Point::new(line.x + 4.0 * ADVANCE + 4.0, line.y + line.height / 2.0),
    );
    let events = h.events();
    let menu = events
        .iter()
        .find_map(|event| match event {
            EditorEvent::SpellMenu {
                range,
                word,
                suggestions,
                anchor,
            } => Some((*range, word.clone(), suggestions.clone(), *anchor)),
            _ => None,
        })
        .expect("a spelling menu");
    assert_eq!(menu.0, TextRange::new(4, 9));
    assert_eq!(menu.1, "quikc");
    assert_eq!(menu.2, ["quick", "quirk"]);
    let anchor = menu.3;
    assert!(close(anchor.x, line.x + 4.0 * ADVANCE), "{anchor:?}");
    assert!(close(anchor.width, 5.0 * ADVANCE), "{anchor:?}");
    assert!(
        close(anchor.y, line.y) && close(anchor.height, line.height),
        "{anchor:?}"
    );
    assert!(
        (4..=9).contains(&h.selection().head),
        "the caret moved into the word"
    );

    // A correct word has no spelling menu.
    right_click(
        &mut h,
        Point::new(line.x + 10.0 * ADVANCE + 4.0, line.y + line.height / 2.0),
    );
    assert!(
        h.events()
            .iter()
            .all(|event| !matches!(event, EditorEvent::SpellMenu { .. }))
    );
}

#[test]
fn adding_or_ignoring_a_word_removes_its_squiggle() {
    let (mut h, provider, spell) = editor("The quikc brown fxo\n", &SENTENCE_WORDS);
    assert_eq!(squiggles(&h, &h.paint()).len(), 2);
    spell.add_to_dictionary("quikc");
    h.view.invalidate_objects();
    assert_eq!(*provider.added.borrow(), ["quikc"]);
    assert_eq!(squiggles(&h, &settled(&h, &spell)).len(), 1);
    spell.ignore("fxo");
    h.view.invalidate_objects();
    assert_eq!(*provider.ignored.borrow(), ["fxo"]);
    assert!(squiggles(&h, &settled(&h, &spell)).is_empty());
}

#[test]
fn choosing_a_suggestion_replaces_the_word_as_one_undo_step() {
    let (mut h, _, spell) = editor(SENTENCE, &SENTENCE_WORDS);
    assert!(SpellCheck::replace(
        &mut h.view,
        TextRange::new(4, 9),
        "quick"
    ));
    assert_eq!(h.text(), "The quick brown fox\n");
    assert_eq!(h.selection().head, 9, "the caret follows the replacement");
    assert!(squiggles(&h, &settled(&h, &spell)).is_empty());
    assert!(h.view.execute(CommandId::EditUndo));
    assert_eq!(h.text(), SENTENCE);
    // The document changed under the menu: nothing happens.
    assert!(!SpellCheck::replace(
        &mut h.view,
        TextRange::new(100, 105),
        "x"
    ));
    assert_eq!(h.text(), SENTENCE);
}

// ----- strings and debug ----------------------------------------------------------------------

#[test]
fn the_menu_labels_come_from_the_fluent_file() {
    let strings = SpellStrings::default();
    assert_eq!(strings.add_to_dictionary, "Add to dictionary");
    assert_eq!(strings.ignore, "Ignore");
    assert_eq!(strings.no_suggestions, "No spelling suggestions");
}

#[test]
fn a_wrapper_can_be_put_around_any_renderer() {
    let provider = Dictionary::new(&[]);
    let spell = Rc::new(SpellCheck::new(provider));
    let renderer = SpellRenderer::new(Rc::new(ParagraphRenderer), Rc::clone(&spell));
    assert!(format!("{renderer:?}").contains("SpellRenderer"));
    let mut h = Harness::new("zzword\n");
    h.view
        .registry_mut()
        .register(BlockClass::Paragraph, Rc::new(renderer));
    let canvas = settled(&h, &spell);
    assert_eq!(squiggles(&h, &canvas).len(), 1);
}

#[test]
fn a_paragraph_without_checkable_text_gets_no_squiggles() {
    let (h, _dictionary, _spell) = editor("`xyzzy`\n", &[]);
    let canvas = h.paint();
    assert!(squiggles(&h, &canvas).is_empty());
}

#[test]
fn a_checked_paragraph_still_answers_selection_lines_links_cursor_and_description() {
    // `hrre` is misspelled: the paragraph keeps its wrapper (a clean one is not wrapped).
    let (mut h, _dictionary, _spell) =
        editor("see [link](http://x.test) hrre\n", &["see", "link", "here"]);
    // The selection is drawn through the wrapped layout.
    h.view.set_selection(Selection::new(0, 3));
    let selection_color = h.theme.tokens.selection_bg;
    let canvas = h.paint();
    assert!(
        canvas.recorded().iter().any(|entry| {
            matches!(&entry.op, DrawOp::FillRect { color, .. } if *color == selection_color)
                && entry
                    .bounds
                    .is_some_and(|b| close(b.x, COLUMN_LEFT) && close(b.width, 3.0 * ADVANCE))
        }),
        "a selection rectangle over the first three characters"
    );
    // End goes to the end of the line.
    h.set_caret(0);
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(30));
    // The link is found and shows the hand with Ctrl held.
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(5),
        0,
        Modifiers::CTRL,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(5),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::IBeam));
    assert_eq!(h.access().description, "Paragraph");
}
