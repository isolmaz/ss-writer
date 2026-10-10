//! The find bar (spec §7): find as you type, navigation, the option toggles, the invalid-regex
//! state, replace and replace all (one undo step, `$1` captures), layout, access tree.

mod search_common;

use std::cell::RefCell;
use std::rc::Rc;

use search_common::{Host, child_names, clipboard, localizer, node_named};
use ssw_core::document::Document;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_core::text::TextRange;
use ssw_ui::access::{AccessAction, Role, ToggleState};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{Modifiers, vk};
use ssw_ui::shell::find::{FindBar, FindEvent, SharedDocument};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

const WINDOW: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);

fn open_bar(doc: &SharedDocument, replace: bool) -> (Host, FindBar) {
    let mut host = Host::new();
    let mut bar =
        FindBar::new(Rc::clone(doc), localizer(), &clipboard()).with_motion(Motion::reduced());
    bar.open(replace);
    host.layout(&mut bar, WINDOW);
    let focus = bar.take_focus_request().expect("opening asks for focus");
    host.focus_on(focus);
    (host, bar)
}

fn setup(text: &str) -> (Host, FindBar, SharedDocument) {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new(text)));
    let (host, bar) = open_bar(&doc, false);
    (host, bar, doc)
}

fn selection(doc: &SharedDocument) -> (usize, usize) {
    let selection = doc.borrow().primary_selection();
    (selection.start(), selection.end())
}

fn range(start: usize, end: usize) -> TextRange {
    TextRange::new(start, end)
}

/// Clicks the control called `name` (a node of the bar's access tree).
fn click_named(host: &mut Host, bar: &mut FindBar, name: &str) {
    let tree = host.tree(bar);
    let at = node_named(&tree, name).bounds.center();
    host.click(bar, at);
}

#[test]
fn typing_selects_the_first_match_at_the_caret_and_counts_the_matches() {
    let (mut host, mut bar, doc) = setup("one two one two one");
    host.text(&mut bar, "one");
    assert_eq!(bar.matches(), &[range(0, 3), range(8, 11), range(16, 19)]);
    assert_eq!(bar.counter_text(), "1/3");
    assert_eq!(bar.current_match(), Some(range(0, 3)));
    assert_eq!(selection(&doc), (0, 3));
    assert_eq!(bar.take_events(), vec![FindEvent::Reveal(range(0, 3))]);
    // Typing on extends the match under the selection instead of jumping on.
    host.text(&mut bar, " two");
    assert_eq!(bar.matches(), &[range(0, 7), range(8, 15)]);
    assert_eq!(selection(&doc), (0, 7));
    assert_eq!(bar.counter_text(), "1/2");
}

#[test]
fn enter_and_f3_go_forward_shift_enter_and_shift_f3_go_back_and_both_wrap() {
    let (mut host, mut bar, doc) = setup("one two one two one");
    host.text(&mut bar, "one");
    host.key(&mut bar, vk::RETURN);
    assert_eq!(selection(&doc), (8, 11));
    assert_eq!(bar.counter_text(), "2/3");
    host.key(&mut bar, vk::F3);
    assert_eq!(selection(&doc), (16, 19));
    assert_eq!(bar.counter_text(), "3/3");
    host.key(&mut bar, vk::F3);
    assert_eq!(selection(&doc), (0, 3));
    host.key_with(&mut bar, vk::F3, Modifiers::SHIFT);
    assert_eq!(selection(&doc), (16, 19));
    assert_eq!(bar.counter_text(), "3/3");
    host.key_with(&mut bar, vk::RETURN, Modifiers::SHIFT);
    assert_eq!(selection(&doc), (8, 11));
    assert_eq!(bar.counter_text(), "2/3");
    // The buttons do the same.
    click_named(&mut host, &mut bar, "Next match");
    assert_eq!(selection(&doc), (16, 19));
    click_named(&mut host, &mut bar, "Previous match");
    assert_eq!(selection(&doc), (8, 11));
    // The host drives the same navigation without the keyboard.
    assert!(bar.find_next());
    assert_eq!(selection(&doc), (16, 19));
    assert!(bar.find_previous());
    assert_eq!(selection(&doc), (8, 11));
}

#[test]
fn next_continues_after_a_caret_moved_by_the_user() {
    let (mut host, mut bar, doc) = setup("aa bb aa bb aa");
    host.text(&mut bar, "aa");
    doc.borrow_mut().set_selections(Selection::caret(7), &[]);
    assert!(bar.find_next());
    assert_eq!(selection(&doc), (12, 14));
    doc.borrow_mut().set_selections(Selection::caret(7), &[]);
    assert!(bar.find_previous());
    assert_eq!(selection(&doc), (6, 8));
}

#[test]
fn the_toggles_narrow_the_search() {
    let (mut host, mut bar, _doc) = setup("Foo foo FOO cat concat cat");
    host.text(&mut bar, "foo");
    assert_eq!(bar.matches().len(), 3);
    let tree = host.tree(&bar);
    let case = node_named(&tree, "Match case");
    assert_eq!(case.role, Role::ToggleButton);
    assert_eq!(case.state.toggle, Some(ToggleState::Off));
    click_named(&mut host, &mut bar, "Match case");
    assert!(bar.query().match_case);
    assert_eq!(bar.matches(), &[range(4, 7)]);
    let tree = host.tree(&bar);
    assert_eq!(
        node_named(&tree, "Match case").state.toggle,
        Some(ToggleState::On)
    );
    bar.set_pattern("cat");
    assert_eq!(
        bar.matches(),
        &[range(12, 15), range(19, 22), range(23, 26)]
    );
    click_named(&mut host, &mut bar, "Whole word");
    assert!(bar.query().whole_word);
    assert_eq!(bar.matches(), &[range(12, 15), range(23, 26)]);
    click_named(&mut host, &mut bar, "Whole word");
    click_named(&mut host, &mut bar, "Regular expression");
    assert!(bar.query().regex);
    bar.set_pattern("c.t");
    assert_eq!(bar.matches().len(), 3);
    assert_eq!(bar.counter_text(), "1/3");
}

#[test]
fn an_invalid_regex_is_shown_as_an_error_not_as_no_results() {
    let (mut host, mut bar, _doc) = setup("a(b");
    click_named(&mut host, &mut bar, "Regular expression");
    // Clicking a toggle moves focus to it; typing goes back to the find box.
    host.focus_on(bar.find_box().id());
    host.text(&mut bar, "(");
    assert!(bar.error().is_some());
    assert!(bar.matches().is_empty());
    assert_eq!(bar.counter_text(), "Invalid regular expression");
    let error = host.theme.tokens.error;
    let canvas = host.paint(&bar);
    assert!(
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == error)),
        "the find box is outlined in the error color"
    );
    let tree = host.tree(&bar);
    assert_eq!(
        node_named(&tree, "Find").description,
        bar.error().expect("an error").to_owned()
    );
    // Fixing the pattern clears the state.
    host.key(&mut bar, vk::BACK);
    assert!(bar.error().is_none());
    assert_eq!(bar.counter_text(), "");
    host.text(&mut bar, r"\(");
    assert_eq!(bar.matches(), &[range(1, 2)]);
}

#[test]
fn no_matches_say_so_and_nothing_navigates_or_replaces() {
    let (mut host, mut bar, doc) = setup("alpha beta");
    assert_eq!(bar.counter_text(), "");
    host.text(&mut bar, "zzz");
    assert_eq!(bar.counter_text(), "No results");
    assert_eq!(bar.current_index(), None);
    assert!(!bar.find_next());
    assert!(!bar.find_previous());
    assert!(!bar.replace_current());
    assert_eq!(bar.replace_all(), 0);
    assert_eq!(doc.borrow().text(), "alpha beta");
    assert!(!doc.borrow().can_undo());
}

#[test]
fn replace_replaces_the_current_match_and_selects_the_next_replace_all_does_the_rest() {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new("foo foo foo foo")));
    let (mut host, mut bar) = open_bar(&doc, true);
    assert!(bar.is_replace_open());
    host.text(&mut bar, "foo");
    let replace_id = bar.replace_box().id();
    host.focus_on(replace_id);
    host.text(&mut bar, "bar");
    assert_eq!(bar.replace_box().text(), "bar");
    bar.take_events();
    click_named(&mut host, &mut bar, "Replace");
    assert_eq!(doc.borrow().text(), "bar foo foo foo");
    assert_eq!(selection(&doc), (4, 7));
    assert_eq!(bar.counter_text(), "1/3");
    assert_eq!(
        bar.take_events(),
        vec![
            FindEvent::Replaced { count: 1 },
            FindEvent::Reveal(range(4, 7))
        ]
    );
    click_named(&mut host, &mut bar, "Replace all");
    assert_eq!(doc.borrow().text(), "bar bar bar bar");
    assert_eq!(bar.take_events(), vec![FindEvent::Replaced { count: 3 }]);
    assert!(bar.matches().is_empty());
    // Replace all was one undo step; the single replace is another.
    assert!(doc.borrow_mut().undo().expect("undo"));
    assert_eq!(doc.borrow().text(), "bar foo foo foo");
    assert!(doc.borrow_mut().undo().expect("undo"));
    assert_eq!(doc.borrow().text(), "foo foo foo foo");
}

#[test]
fn enter_in_the_replace_box_replaces_and_ctrl_enter_replaces_all() {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new("a a a")));
    let (mut host, mut bar) = open_bar(&doc, true);
    host.text(&mut bar, "a");
    let replace_id = bar.replace_box().id();
    host.focus_on(replace_id);
    host.text(&mut bar, "b");
    host.key(&mut bar, vk::RETURN);
    assert_eq!(doc.borrow().text(), "b a a");
    host.key_with(&mut bar, vk::RETURN, Modifiers::CTRL);
    assert_eq!(doc.borrow().text(), "b b b");
}

#[test]
fn regex_replace_all_expands_capture_groups_in_one_undo_step() {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new("John Smith, Jane Doe")));
    let (mut host, mut bar) = open_bar(&doc, true);
    click_named(&mut host, &mut bar, "Regular expression");
    host.focus_on(bar.find_box().id());
    host.text(&mut bar, r"(\w+) (\w+)");
    let replace_id = bar.replace_box().id();
    host.focus_on(replace_id);
    host.text(&mut bar, "$2 $1");
    host.key_with(&mut bar, vk::RETURN, Modifiers::CTRL);
    assert_eq!(doc.borrow().text(), "Smith John, Doe Jane");
    assert!(
        bar.take_events()
            .contains(&FindEvent::Replaced { count: 2 })
    );
    assert!(doc.borrow_mut().undo().expect("undo"));
    assert_eq!(doc.borrow().text(), "John Smith, Jane Doe");
    assert!(!doc.borrow().can_undo());
}

#[test]
fn a_dollar_is_literal_outside_regex_mode() {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new("price price")));
    let (mut host, mut bar) = open_bar(&doc, true);
    host.text(&mut bar, "price");
    bar.set_replacement("$1");
    assert_eq!(bar.replace_all(), 2);
    assert_eq!(doc.borrow().text(), "$1 $1");
}

#[test]
fn escape_closes_the_bar_and_hands_focus_back() {
    let (mut host, mut bar, _doc) = setup("text");
    assert!(host.key(&mut bar, vk::ESCAPE).handled);
    assert!(!bar.is_visible());
    assert_eq!(bar.take_events(), vec![FindEvent::Closed]);
    host.layout(&mut bar, WINDOW);
    assert!(host.focus.tab_order().is_empty());
    assert_eq!(host.focus.focused(), None);
    assert!(host.paint(&bar).recorded().is_empty());
    let tree = host.tree(&bar);
    let root = tree.root().expect("root");
    assert!(root.state.offscreen);
    assert!(root.children.is_empty());
    // A hidden bar ignores input.
    assert!(!host.key(&mut bar, vk::F3).handled);
    assert!(!host.text(&mut bar, "x").handled);
    assert!(!bar.find_next());
}

#[test]
fn keys_outside_the_bar_are_not_taken() {
    let (mut host, mut bar, _doc) = setup("text");
    host.focus.blur();
    assert!(!host.key(&mut bar, vk::ESCAPE).handled);
    assert!(bar.is_visible());
    assert!(!host.key(&mut bar, vk::F3).handled);
}

#[test]
fn the_close_button_and_clicks_on_the_panel_behave() {
    let (mut host, mut bar, _doc) = setup("text");
    let panel = bar.panel();
    let inside = Point::new(panel.x + 1.0, panel.y + 1.0);
    assert!(host.click(&mut bar, inside).handled);
    assert!(!host.click(&mut bar, Point::new(10.0, 10.0)).handled);
    click_named(&mut host, &mut bar, "Close");
    assert!(!bar.is_visible());
    assert_eq!(bar.take_events(), vec![FindEvent::Closed]);
}

#[test]
fn the_bar_floats_at_the_top_right_and_grows_a_replace_row() {
    let (mut host, mut bar, _doc) = setup("text");
    assert_eq!(
        bar.panel(),
        Rect::new(800.0 - 12.0 - 520.0, 12.0, 520.0, 40.0)
    );
    let texts_find = host.paint(&bar);
    let texts = texts_find.texts();
    assert!(texts.contains(&"Aa") && texts.contains(&"ab") && texts.contains(&".*"));
    assert!(!texts.contains(&"Replace all"));
    // Ctrl+H adds the second row.
    bar.open(true);
    host.layout(&mut bar, WINDOW);
    assert_eq!(
        bar.panel(),
        Rect::new(800.0 - 12.0 - 520.0, 12.0, 520.0, 80.0)
    );
    let canvas = host.paint(&bar);
    let texts = canvas.texts();
    assert!(texts.contains(&"Replace") && texts.contains(&"Replace all"));
    // A narrow editor shrinks the bar.
    host.layout(&mut bar, Rect::new(0.0, 0.0, 300.0, 400.0));
    assert_eq!(bar.panel().width, 276.0);
    assert_eq!(bar.panel().right(), 288.0);
}

#[test]
fn the_access_tree_names_every_control_and_the_counter() {
    let (mut host, mut bar, _doc) = setup("one two one");
    let tree = host.tree(&bar);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.role, Role::Group);
    assert_eq!(root.name, "Find and replace");
    assert_eq!(
        child_names(&tree, root.id),
        [
            "Toggle replace",
            "Find",
            "Match case",
            "Whole word",
            "Regular expression",
            "",
            "Previous match",
            "Next match",
            "Close"
        ]
    );
    host.text(&mut bar, "one");
    let tree = host.tree(&bar);
    let find = node_named(&tree, "Find");
    assert_eq!(find.role, Role::Edit);
    assert_eq!(find.value.as_deref(), Some("one"));
    assert!(find.state.focused);
    let counter = tree
        .children(root.id)
        .find(|node| node.role == Role::Text)
        .expect("counter");
    assert_eq!(counter.name, "1/2");
    assert_eq!(node_named(&tree, "Next match").role, Role::Button);
    // With the replace row open, its controls join the tree.
    bar.open(true);
    host.layout(&mut bar, WINDOW);
    let tree = host.tree(&bar);
    let names = child_names(&tree, root.id);
    assert_eq!(
        &names[names.len() - 3..],
        ["Replace with", "Replace", "Replace all"]
    );
    assert_eq!(
        node_named(&tree, "Toggle replace").state.toggle,
        Some(ToggleState::On)
    );
}

#[test]
fn assistive_technology_can_toggle_and_focus_the_controls() {
    let (mut host, mut bar, _doc) = setup("Aa aa");
    host.text(&mut bar, "aa");
    assert_eq!(bar.matches().len(), 2);
    let tree = host.tree(&bar);
    let case_id = node_named(&tree, "Match case").id;
    let next_id = node_named(&tree, "Next match").id;
    assert!(bar.owns(case_id) && bar.owns(bar.id()));
    assert!(!bar.owns(ssw_ui::widget::WidgetId::allocate()));
    let mut cx = host.cx(bar.id());
    assert!(bar.perform_child(&mut cx, case_id, &AccessAction::Toggle));
    assert!(bar.query().match_case);
    assert_eq!(bar.matches(), &[range(3, 5)]);
    assert!(bar.perform_child(&mut cx, next_id, &AccessAction::Invoke));
    assert!(!bar.perform_child(&mut cx, next_id, &AccessAction::Toggle));
    assert!(!bar.perform_child(
        &mut cx,
        ssw_ui::widget::WidgetId::allocate(),
        &AccessAction::Invoke
    ));
    // Setting the value of the find box through the tree searches again.
    let find_id = node_named(&host.tree(&bar), "Find").id;
    let mut cx = host.cx(bar.id());
    assert!(bar.perform_child(&mut cx, find_id, &AccessAction::SetValue("Aa".into())));
    assert_eq!(bar.matches(), &[range(0, 2)]);
    // A hidden bar refuses.
    bar.close();
    let mut cx = host.cx(bar.id());
    assert!(!bar.perform_child(&mut cx, find_id, &AccessAction::Focus));
}

#[test]
fn matches_follow_edits_made_elsewhere() {
    let (mut host, mut bar, doc) = setup("one");
    host.text(&mut bar, "one");
    assert_eq!(bar.matches().len(), 1);
    let before = doc.borrow().primary_selection();
    doc.borrow_mut()
        .apply(Transaction::single(
            Edit::insert(0, "one "),
            before,
            Selection::caret(4),
        ))
        .expect("edit");
    bar.refresh();
    assert_eq!(bar.matches(), &[range(0, 3), range(4, 7)]);
    assert_eq!(bar.counter_text(), "2/2");
    // Moving the caret only moves the counter.
    doc.borrow_mut().set_selections(Selection::caret(0), &[]);
    bar.refresh();
    assert_eq!(bar.counter_text(), "1/2");
    // Another document can be searched.
    let other: SharedDocument = Rc::new(RefCell::new(Document::new("none one one")));
    bar.set_document(other);
    assert_eq!(bar.matches().len(), 3);
}

#[test]
fn a_single_line_selection_seeds_the_find_box() {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new("hello world\nsecond line")));
    doc.borrow_mut().set_selections(Selection::new(6, 11), &[]);
    let (_host, bar) = open_bar(&doc, false);
    assert_eq!(bar.find_box().text(), "world");
    assert_eq!(bar.find_box().selection(), 0..5);
    assert_eq!(bar.matches(), &[range(6, 11)]);
    // A multi-line selection does not.
    let multi: SharedDocument = Rc::new(RefCell::new(Document::new("a\nb")));
    multi.borrow_mut().set_selections(Selection::new(0, 3), &[]);
    let (_host, bar) = open_bar(&multi, false);
    assert_eq!(bar.find_box().text(), "");
}

#[test]
fn options_set_by_the_host_search_again() {
    let (_host, mut bar, doc) = setup("Cat cat");
    bar.set_pattern("cat");
    assert_eq!(bar.matches().len(), 2);
    bar.set_options(true, false, false);
    assert_eq!(bar.matches(), &[range(4, 7)]);
    assert_eq!(selection(&doc), (4, 7));
    assert!(bar.query().match_case && !bar.query().regex);
}
