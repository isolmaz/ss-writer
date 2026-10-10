//! Read-only mode of the editor view (the Explorer preview): nothing changes the text, the
//! caret is not drawn, while selection, navigation and copy keep working.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;

use editor_common::{Harness, KEY_A, KEY_C, KEY_V, KEY_X, KEY_Z};
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_ui::canvas::DrawOp;
use ssw_ui::input::{Modifiers, vk};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::widgets::SharedClipboard;

const TEXT: &str = "alpha beta\n\ngamma\n";

fn read_only(text: &str) -> Harness {
    let mut h = Harness::new(text);
    h.view.set_read_only(true);
    h
}

#[test]
fn the_flag_is_off_by_default_and_reported() {
    let mut h = Harness::new(TEXT);
    assert!(!h.view.is_read_only());
    h.view.set_read_only(true);
    assert!(h.view.is_read_only());
    h.view.set_read_only(false);
    assert!(!h.view.is_read_only());
}

#[test]
fn typing_deleting_and_commands_leave_the_text_and_version_untouched() {
    let mut h = read_only(TEXT);
    let version = h.doc.borrow().version();
    h.set_caret(5);
    h.type_text("x");
    h.press(vk::BACK);
    h.press(vk::DELETE);
    h.press(vk::RETURN);
    h.press(vk::TAB);
    h.key(0x42, Modifiers::CTRL); // Ctrl+B
    assert!(h.view.execute(CommandId::FormatBold));
    assert!(h.view.execute(CommandId::FormatCodeBlock));
    assert_eq!(h.text(), TEXT);
    assert_eq!(h.doc.borrow().version(), version);
}

#[test]
fn undo_and_redo_are_owned_but_do_nothing() {
    let mut h = Harness::new(TEXT);
    h.set_caret(5);
    h.type_text("x");
    assert_eq!(h.text(), "alphax beta\n\ngamma\n");
    h.view.set_read_only(true);
    assert!(h.view.execute(CommandId::EditUndo));
    assert_eq!(h.text(), "alphax beta\n\ngamma\n", "undo is refused");
    h.key(KEY_Z, Modifiers::CTRL);
    assert_eq!(h.text(), "alphax beta\n\ngamma\n");
    h.view.set_read_only(false);
    h.key(KEY_Z, Modifiers::CTRL);
    assert_eq!(h.text(), TEXT, "the same undo works again once editable");
}

#[test]
fn paste_and_cut_do_not_change_the_text_but_cut_still_copies() {
    let mut h = read_only(TEXT);
    let clip: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    h.view.set_clipboard(Rc::clone(&clip));
    h.view.set_selection(Selection::new(0, 5));
    h.key(KEY_X, Modifiers::CTRL);
    assert_eq!(clip.borrow().text().unwrap().as_deref(), Some("alpha"));
    assert_eq!(h.text(), TEXT, "cut removes nothing");
    h.view.set_selection(Selection::new(6, 10));
    h.key(KEY_C, Modifiers::CTRL);
    assert_eq!(clip.borrow().text().unwrap().as_deref(), Some("beta"));
    h.key(KEY_V, Modifiers::CTRL);
    assert_eq!(h.text(), TEXT, "paste inserts nothing");
}

#[test]
fn selection_and_navigation_still_work() {
    let mut h = read_only(TEXT);
    h.key(KEY_A, Modifiers::CTRL);
    assert_eq!(h.selection(), Selection::new(0, TEXT.len()));
    // Select all leaves the head at the end of the text, so Home alone would go to the start of
    // the last line: Ctrl+Home is the document start.
    h.key(vk::HOME, Modifiers::CTRL);
    assert_eq!(h.selection(), Selection::caret(0));
    h.press(vk::RIGHT);
    assert_eq!(h.selection(), Selection::caret(1));
    h.key(vk::END, Modifiers::CTRL);
    assert_eq!(h.selection().head, TEXT.len());
}

#[test]
fn markers_stay_hidden_under_the_caret_and_the_selection() {
    let mut h = Harness::new("a **bold** b\n");
    h.set_caret(4);
    assert_eq!(
        h.painted_texts(),
        ["a **bold** b"],
        "editable: the caret reveals"
    );
    h.view.set_read_only(true);
    assert_eq!(
        h.painted_texts(),
        ["a bold b"],
        "read-only: nothing is revealed"
    );
    h.view.set_selection(Selection::new(0, 12));
    assert_eq!(
        h.painted_texts(),
        ["a bold b"],
        "a selection does not reveal either"
    );
}

#[test]
fn the_caret_is_not_drawn() {
    let mut h = Harness::new("abc\n");
    h.set_caret(2);
    let caret_color = h.theme.tokens.caret;
    let carets = |h: &Harness| {
        h.paint()
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRect { color, .. } if *color == caret_color))
            .count()
    };
    assert_eq!(carets(&h), 1, "an editable focused view draws its caret");
    h.view.set_read_only(true);
    assert_eq!(carets(&h), 0);
}
