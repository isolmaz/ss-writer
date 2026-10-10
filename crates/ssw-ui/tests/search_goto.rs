//! Go to line (`Ctrl+G`): seeding, `line` and `line:column`, clamping, invalid input, closing,
//! access tree.

mod search_common;

use std::cell::RefCell;
use std::rc::Rc;

use search_common::{Host, child_names, clipboard, localizer, node_named};
use ssw_core::document::Document;
use ssw_core::edit::Selection;
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::vk;
use ssw_ui::shell::find::SharedDocument;
use ssw_ui::shell::goto::{GoToEvent, GoToLine};
use ssw_ui::widget::{Widget, WidgetId};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

const WINDOW: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);

fn setup(text: &str, caret: usize) -> (Host, GoToLine, SharedDocument) {
    let doc: SharedDocument = Rc::new(RefCell::new(Document::new(text)));
    doc.borrow_mut()
        .set_selections(Selection::caret(caret), &[]);
    let mut host = Host::new();
    let mut goto =
        GoToLine::new(Rc::clone(&doc), localizer(), &clipboard()).with_motion(Motion::reduced());
    goto.open();
    host.layout(&mut goto, WINDOW);
    let focus = goto.take_focus_request().expect("opening asks for focus");
    host.focus_on(focus);
    (host, goto, doc)
}

fn caret(doc: &SharedDocument) -> usize {
    doc.borrow().primary_selection().head
}

#[test]
fn it_opens_at_the_top_centre_with_the_current_line_selected() {
    let (_host, goto, _doc) = setup("a\nbb\nccc", 4);
    assert_eq!(goto.input().text(), "2");
    assert_eq!(goto.input().selection(), 0..1);
    assert_eq!(goto.hint_text(), "Type a line number between 1 and 3");
    assert!(!goto.is_invalid());
    assert_eq!(goto.panel(), Rect::new(220.0, 48.0, 360.0, 76.0));
}

#[test]
fn typing_replaces_the_line_and_enter_moves_the_caret_and_closes() {
    let (mut host, mut goto, doc) = setup("a\nbb\nccc", 4);
    host.text(&mut goto, "3");
    assert_eq!(goto.input().text(), "3");
    assert!(host.key(&mut goto, vk::RETURN).handled);
    assert_eq!(caret(&doc), 5);
    assert!(doc.borrow().primary_selection().is_caret());
    assert_eq!(goto.take_events(), vec![GoToEvent::Went { offset: 5 }]);
    assert!(!goto.is_visible());
    host.layout(&mut goto, WINDOW);
    assert_eq!(host.focus.focused(), None);
}

#[test]
fn a_column_counts_characters_and_everything_clamps() {
    let (_host, mut goto, doc) = setup("a\nbb\nccc", 0);
    goto.set_input("2:2");
    assert!(goto.submit());
    assert_eq!(caret(&doc), 3);
    goto.open();
    goto.set_input("1:99");
    assert!(goto.submit());
    assert_eq!(caret(&doc), 1);
    goto.open();
    goto.set_input("99");
    assert!(goto.submit());
    assert_eq!(caret(&doc), 5);
    goto.open();
    goto.set_input(" 3 : 2 ");
    assert!(goto.submit());
    assert_eq!(caret(&doc), 6);
    // Columns are characters, not bytes: "çay" starts at byte 3, its `a` is byte 2 further.
    let (_host, mut goto, doc) = setup("é\nçay", 0);
    goto.set_input("2:2");
    assert!(goto.submit());
    assert_eq!(caret(&doc), 5);
}

#[test]
fn text_that_is_not_a_position_is_flagged_and_does_not_move_anything() {
    let (mut host, mut goto, doc) = setup("a\nbb\nccc", 4);
    for bad in ["abc", "0", "1:0", ":", "1:x", "-1"] {
        goto.set_input(bad);
        assert!(goto.is_invalid(), "{bad:?}");
        assert_eq!(goto.hint_text(), "Not a valid line number");
        assert!(!goto.submit(), "{bad:?}");
    }
    goto.set_input("abc");
    host.key(&mut goto, vk::RETURN);
    assert!(goto.is_visible());
    assert_eq!(caret(&doc), 4);
    assert!(goto.take_events().is_empty());
    let error = host.theme.tokens.error;
    let canvas = host.paint(&goto);
    assert!(
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == error))
    );
    assert!(canvas.texts().contains(&"Not a valid line number"));
    // An empty box is not an error, but there is nowhere to go.
    goto.set_input("");
    assert!(!goto.is_invalid());
    assert!(!goto.submit());
}

#[test]
fn escape_and_a_press_outside_close_without_moving() {
    let (mut host, mut goto, doc) = setup("a\nbb", 2);
    assert!(host.key(&mut goto, vk::ESCAPE).handled);
    assert!(!goto.is_visible());
    assert_eq!(goto.take_events(), vec![GoToEvent::Closed]);
    assert_eq!(caret(&doc), 2);
    // Closing twice reports once; a hidden overlay ignores everything.
    goto.close();
    assert!(goto.take_events().is_empty());
    assert!(!host.key(&mut goto, vk::RETURN).handled);
    goto.open();
    host.layout(&mut goto, WINDOW);
    let inside = Point::new(goto.panel().x + 2.0, goto.panel().y + 2.0);
    assert!(host.click(&mut goto, inside).handled);
    assert!(goto.is_visible());
    assert!(!host.click(&mut goto, Point::new(5.0, 500.0)).handled);
    assert!(!goto.is_visible());
    assert_eq!(goto.take_events(), vec![GoToEvent::Closed]);
}

#[test]
fn a_key_the_box_does_not_use_leaves_the_overlay_open_and_untaken() {
    let (mut host, mut goto, doc) = setup("a\nbb", 2);
    assert!(!host.key(&mut goto, vk::F3).handled);
    assert!(goto.is_visible());
    assert!(goto.take_events().is_empty());
    assert_eq!(caret(&doc), 2);
}

#[test]
fn the_access_tree_has_the_box_and_the_hint() {
    let (mut host, mut goto, _doc) = setup("a\nbb\nccc", 0);
    let tree = host.tree(&goto);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.role, Role::Group);
    assert_eq!(root.name, "Go to line");
    assert_eq!(
        child_names(&tree, root.id),
        ["Go to line", "Type a line number between 1 and 3"]
    );
    let input = node_named(&tree, "Go to line");
    assert_eq!(input.role, Role::Edit);
    assert_eq!(input.value.as_deref(), Some("1"));
    assert_eq!(input.description, "Type a line number between 1 and 3");
    // Assistive technology sets the value; the hint follows.
    let input_id = input.id;
    assert!(goto.owns(input_id) && goto.owns(goto.id()));
    assert!(!goto.owns(WidgetId::allocate()));
    let mut cx = host.cx(goto.id());
    assert!(goto.perform_child(&mut cx, input_id, &AccessAction::SetValue("x".into())));
    assert!(!goto.perform_child(&mut cx, WidgetId::allocate(), &AccessAction::Focus));
    // An action the box refuses is refused by the overlay.
    assert!(!goto.perform_child(&mut cx, input_id, &AccessAction::Invoke));
    assert!(goto.is_invalid());
    let tree = host.tree(&goto);
    assert_eq!(
        node_named(&tree, "Go to line").description,
        "Not a valid line number"
    );
    // Hidden: an offscreen group and no actions.
    goto.close();
    let tree = host.tree(&goto);
    assert!(tree.root().expect("root").state.offscreen);
    let mut cx = host.cx(goto.id());
    assert!(!goto.perform_child(&mut cx, input_id, &AccessAction::Focus));
}

#[test]
fn another_document_can_be_navigated() {
    let (_host, mut goto, _doc) = setup("one", 0);
    let other: SharedDocument = Rc::new(RefCell::new(Document::new("x\ny\nz")));
    goto.set_document(Rc::clone(&other));
    goto.open();
    assert_eq!(goto.hint_text(), "Type a line number between 1 and 3");
    goto.set_input("3");
    assert!(goto.submit());
    assert_eq!(caret(&other), 4);
}

#[test]
fn the_overlay_measures_its_panel_and_nothing_while_hidden() {
    let (mut host, mut goto, _doc) = setup("a\nbb", 0);
    assert_eq!(
        host.measure(&mut goto, Size::new(800.0, 600.0)),
        Size::new(360.0, 76.0)
    );
    // A narrow editor leaves the padding on both sides; an unbounded one gets the widest panel.
    assert_eq!(
        host.measure(&mut goto, Size::new(200.0, 600.0)),
        Size::new(184.0, 76.0)
    );
    assert_eq!(
        host.measure(&mut goto, Size::UNBOUNDED),
        Size::new(360.0, 76.0)
    );
    goto.close();
    assert_eq!(host.measure(&mut goto, Size::new(800.0, 600.0)), Size::ZERO);
}

#[test]
fn a_valid_position_gets_a_muted_hint_and_painting_follows_visibility_and_damage() {
    let (mut host, mut goto, _doc) = setup("a\nbb\nccc", 0);
    let muted = host.theme.tokens.text_muted;
    let canvas = host.paint(&goto);
    let hint = canvas.ops().find_map(|op| match op {
        DrawOp::Text { text, color, .. } if text == "Type a line number between 1 and 3" => {
            Some(*color)
        }
        _ => None,
    });
    assert_eq!(hint, Some(muted));
    // Nothing is drawn outside the damaged area.
    let away = Rect::new(0.0, 400.0, 100.0, 100.0);
    assert_eq!(host.paint_damaged(&goto, Some(away)).ops().count(), 0);
    let over = goto.panel();
    assert!(host.paint_damaged(&goto, Some(over)).ops().count() > 0);
    // A hidden overlay draws nothing at all.
    goto.close();
    host.layout(&mut goto, WINDOW);
    assert_eq!(host.paint(&goto).ops().count(), 0);
}

#[test]
fn clicking_the_box_takes_the_focus_back() {
    let (mut host, mut goto, _doc) = setup("a\nbb", 0);
    host.focus.blur();
    assert_eq!(host.focus.focused(), None);
    let at = goto.input().bounds().center();
    assert!(host.click(&mut goto, at).handled);
    assert_eq!(host.focus.focused(), Some(goto.input().id()));
}

#[test]
fn the_overlay_itself_takes_no_accessibility_actions() {
    let (mut host, mut goto, _doc) = setup("a", 0);
    let id = goto.id();
    let mut cx = host.cx(id);
    for action in [AccessAction::Invoke, AccessAction::Focus] {
        assert!(!Widget::perform(&mut goto, &mut cx, &action));
    }
}
