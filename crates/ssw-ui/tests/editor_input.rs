//! Editing through the view: typing, deletion, Enter/Tab behaviors, navigation, undo grouping,
//! commands and the clipboard. Every assertion is on the document text or the selection after
//! the event, i.e. on what the `ssw_core::ops` produced.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use editor_common::{Harness, KEY_A, KEY_C, KEY_V, KEY_X, KEY_Z};
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_ui::editor::EditorEvent;
use ssw_ui::input::{Modifiers, vk};
use ssw_ui::services::{ClipboardContent, MemoryClipboard};
use ssw_ui::widgets::SharedClipboard;

fn ctrl() -> Modifiers {
    Modifiers::CTRL
}

fn clipboard(h: &mut Harness) -> SharedClipboard {
    let clip: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    h.view.set_clipboard(Rc::clone(&clip));
    clip
}

#[test]
fn typed_text_goes_through_the_ops_and_groups_into_undo_steps() {
    let mut h = Harness::new("");
    assert!(h.type_text("a").handled);
    h.type_text("b");
    assert_eq!(h.text(), "ab");
    assert_eq!(h.selection(), Selection::caret(2));
    // A long pause starts a new undo step.
    h.clock.advance(Duration::from_secs(2));
    h.type_text("c");
    assert_eq!(h.text(), "abc");
    h.key(KEY_Z, ctrl());
    assert_eq!(h.text(), "ab", "the pause ended the run");
    h.key(KEY_Z, ctrl());
    assert_eq!(h.text(), "", "the first run is one step");
    h.key(KEY_Z, ctrl());
    assert_eq!(h.text(), "", "nothing left to undo");
    h.key(KEY_Z, ctrl() | Modifiers::SHIFT);
    assert_eq!(h.text(), "ab", "Ctrl+Shift+Z redoes");
    h.key(0x59, ctrl());
    assert_eq!(h.text(), "abc", "Ctrl+Y redoes");
}

#[test]
fn control_characters_in_text_events_are_ignored() {
    let mut h = Harness::new("x");
    assert!(!h.type_text("\u{8}").handled);
    assert!(!h.type_text("").handled);
    assert_eq!(h.text(), "x");
    assert!(h.type_text("tab\there\nnewline").handled);
    assert_eq!(h.text(), "tab\there\nnewlinex");
}

#[test]
fn keys_are_ignored_without_focus_and_altgr_is_text_not_a_command() {
    let mut h = Harness::new("abc");
    h.key(KEY_A, ctrl() | Modifiers::ALT);
    assert_eq!(
        h.selection(),
        Selection::caret(0),
        "Ctrl+Alt+A is not select all"
    );
    h.focus.blur();
    assert!(!h.press(vk::RIGHT).handled);
    assert!(!h.type_text("z").handled);
    assert_eq!(h.text(), "abc");
}

#[test]
fn backspace_delete_and_word_deletion() {
    let mut h = Harness::new("one two three");
    h.set_caret(3);
    h.press(vk::BACK);
    assert_eq!(h.text(), "on two three");
    assert_eq!(h.selection(), Selection::caret(2));
    h.press(vk::DELETE);
    assert_eq!(h.text(), "ontwo three");
    h.set_caret(11);
    h.key(vk::BACK, ctrl());
    assert_eq!(h.text(), "ontwo ");
    h.view.set_selection(Selection::new(0, 6));
    h.press(vk::DELETE);
    assert_eq!(h.text(), "", "a selection is deleted whole");
    assert!(
        h.press(vk::BACK).handled,
        "backspace at the start is consumed"
    );
    assert_eq!(h.text(), "");
    let mut h = Harness::new("one two");
    h.key(vk::DELETE, ctrl());
    assert_eq!(h.text(), "two", "Ctrl+Delete removes up to the next word");
}

#[test]
fn enter_continues_lists_and_tab_indents_items() {
    let mut h = Harness::new("- a");
    h.set_caret(3);
    h.press(vk::RETURN);
    assert_eq!(h.text(), "- a\n- ");
    h.type_text("b");
    assert_eq!(h.text(), "- a\n- b");
    h.press(vk::TAB);
    assert_eq!(h.text(), "- a\n  - b");
    h.key(vk::TAB, Modifiers::SHIFT);
    assert_eq!(h.text(), "- a\n- b");
    // Enter on an empty item leaves the list.
    h.press(vk::RETURN);
    h.press(vk::RETURN);
    assert_eq!(h.text(), "- a\n- b\n");
}

#[test]
fn shift_enter_and_plain_enter_in_paragraphs() {
    let mut h = Harness::new("ab");
    h.set_caret(1);
    h.press(vk::RETURN);
    assert_eq!(h.text(), "a\nb");
    h.set_caret(3);
    h.key(vk::RETURN, Modifiers::SHIFT);
    assert_eq!(h.text(), "a\nb\n");
}

#[test]
fn tab_in_a_paragraph_inserts_a_tab() {
    let mut h = Harness::new("ab");
    h.set_caret(1);
    h.press(vk::TAB);
    assert_eq!(h.text(), "a\tb");
}

#[test]
fn tab_moves_between_table_cells() {
    let mut h = Harness::new("| a | b |\n|---|---|\n| 1 | 2 |\n");
    h.set_caret(2);
    h.press(vk::TAB);
    let head = h.selection().head;
    assert!(
        (5..=8).contains(&head),
        "the second cell of the header: {head}"
    );
    assert_eq!(h.text(), "| a | b |\n|---|---|\n| 1 | 2 |\n");
    h.key(vk::TAB, Modifiers::SHIFT);
    assert!(h.selection().head < 5, "back in the first cell");
}

#[test]
fn horizontal_and_document_navigation_with_selection_extension() {
    let mut h = Harness::new("one two three\n\nsecond paragraph\n");
    h.key(vk::RIGHT, ctrl());
    assert_eq!(h.selection(), Selection::caret(4), "next word");
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(13), "end of the line");
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(0), "start of the line");
    h.key(vk::DOWN, ctrl());
    assert_eq!(h.selection(), Selection::caret(15), "next paragraph");
    h.key(vk::UP, ctrl());
    assert_eq!(h.selection(), Selection::caret(0), "previous paragraph");
    h.key(vk::END, ctrl());
    assert_eq!(h.selection(), Selection::caret(32), "end of the document");
    h.key(vk::HOME, ctrl());
    assert_eq!(h.selection(), Selection::caret(0));
    h.key(vk::RIGHT, Modifiers::SHIFT);
    assert_eq!(h.selection(), Selection::new(0, 1));
    h.key(vk::RIGHT, ctrl() | Modifiers::SHIFT);
    assert_eq!(h.selection(), Selection::new(0, 4), "word-wise extension");
    h.press(vk::LEFT);
    assert_eq!(
        h.selection(),
        Selection::caret(0),
        "left collapses to the start"
    );
    h.press(vk::ESCAPE);
    assert!(
        !h.press(vk::ESCAPE).handled,
        "Escape with a caret is not ours"
    );
}

#[test]
fn escape_collapses_a_selection() {
    let mut h = Harness::new("abc");
    h.view.set_selection(Selection::new(0, 2));
    assert!(h.press(vk::ESCAPE).handled);
    assert_eq!(h.selection(), Selection::caret(2));
}

#[test]
fn vertical_navigation_visits_blank_lines_and_keeps_the_goal_column() {
    let mut h = Harness::new("aaaa\n\nbbbb\n");
    h.set_caret(2);
    h.press(vk::DOWN);
    assert_eq!(h.selection(), Selection::caret(5), "the blank line");
    h.press(vk::DOWN);
    assert_eq!(
        h.selection(),
        Selection::caret(8),
        "the goal column survives the blank line"
    );
    h.press(vk::UP);
    assert_eq!(h.selection(), Selection::caret(5));
    h.press(vk::UP);
    assert_eq!(h.selection(), Selection::caret(2));
    h.press(vk::UP);
    assert_eq!(
        h.selection(),
        Selection::caret(0),
        "up on the first line goes to the start"
    );
    h.key(vk::END, ctrl());
    h.press(vk::DOWN);
    assert_eq!(
        h.selection(),
        Selection::caret(11),
        "down on the last line goes to the end"
    );
}

#[test]
fn home_end_and_vertical_movement_follow_wrapped_lines() {
    // 44 columns fit the 352 DIP column: 100 characters wrap into 44 + 44 + 12.
    let mut h = Harness::new(&format!("{}\n", "x".repeat(100)));
    h.set_caret(50);
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(44));
    h.set_caret(50);
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(88));
    h.set_caret(50);
    h.press(vk::UP);
    assert_eq!(h.selection(), Selection::caret(6));
    h.press(vk::DOWN);
    h.press(vk::DOWN);
    assert_eq!(h.selection(), Selection::caret(94));
    h.key(vk::DOWN, Modifiers::SHIFT);
    assert_eq!(h.selection().anchor, 94, "shift keeps the anchor");
}

#[test]
fn page_keys_move_by_a_viewport() {
    let text: String = (0..60).map(|i| format!("line {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    h.press(vk::PAGE_DOWN);
    let after_down = h.selection().head;
    assert!(after_down > 0);
    assert!(h.view.scroll_y() > 0.0, "the view followed");
    h.press(vk::PAGE_UP);
    assert!(h.selection().head < after_down);
}

#[test]
fn navigation_and_edits_report_events() {
    let mut h = Harness::new("ab");
    h.type_text("x");
    let events = h.events();
    assert!(events.contains(&EditorEvent::Edited));
    assert!(events.contains(&EditorEvent::SelectionChanged(Selection::caret(1))));
    h.press(vk::RIGHT);
    assert_eq!(
        h.events(),
        [EditorEvent::SelectionChanged(Selection::caret(2))]
    );
    h.press(vk::END);
    h.events();
    h.press(vk::END);
    assert!(h.events().is_empty(), "no change, no event");
}

#[test]
fn format_commands_wrap_unwrap_and_undo_as_one_step() {
    let mut h = Harness::new("bold word");
    h.view.set_selection(Selection::new(0, 4));
    assert!(h.view.execute(CommandId::FormatBold));
    assert_eq!(h.text(), "**bold** word");
    assert!(
        h.view
            .style_active(ssw_core::ops::format::InlineStyle::Bold)
    );
    h.view.execute(CommandId::FormatBold);
    assert_eq!(h.text(), "bold word", "toggling again unwraps");
    h.view.execute(CommandId::EditUndo);
    assert_eq!(h.text(), "**bold** word", "unwrap was one step");
    h.view.execute(CommandId::EditUndo);
    assert_eq!(h.text(), "bold word", "wrapping was one step");
    h.view.execute(CommandId::EditRedo);
    assert_eq!(h.text(), "**bold** word");
}

#[test]
fn block_commands_use_the_ops() {
    let mut h = Harness::new("text");
    h.set_caret(2);
    assert!(h.view.execute(CommandId::FormatHeading2));
    assert_eq!(h.text(), "## text");
    h.view.execute(CommandId::FormatParagraph);
    assert_eq!(h.text(), "text");
    h.view.execute(CommandId::FormatQuote);
    assert_eq!(h.text(), "> text");
    h.view.execute(CommandId::FormatQuote);
    assert_eq!(h.text(), "text");
    h.view.execute(CommandId::FormatBulletList);
    assert_eq!(h.text(), "- text");
    h.view.execute(CommandId::FormatTaskToggle);
    h.view.execute(CommandId::FormatTaskList);
    assert!(h.text().contains("[ ]"), "{}", h.text());
    h.view.execute(CommandId::FormatTaskToggle);
    assert!(h.text().contains("[x]"), "{}", h.text());
}

#[test]
fn commands_the_app_owns_are_not_handled() {
    let mut h = Harness::new("text");
    for command in [
        CommandId::FileSave,
        CommandId::SearchFind,
        CommandId::FormatLink,
        CommandId::EditCopyAsHtml,
        CommandId::ViewFoldAll,
    ] {
        assert!(!h.view.execute(command), "{command}");
    }
    assert!(
        !h.view.execute(CommandId::EditCopy),
        "no clipboard attached"
    );
    assert_eq!(h.text(), "text");
}

#[test]
fn insert_date_link_image_and_pastes_use_the_ops() {
    let mut h = Harness::new("");
    h.view.insert_date("1. March 2026");
    assert_eq!(h.text(), "1\\. March 2026");
    let mut h = Harness::new("see word");
    h.view.set_selection(Selection::new(4, 8));
    h.view.set_link("word", "https://x.y");
    assert_eq!(h.text(), "see [word](https://x.y)");
    h.set_caret(7);
    assert_eq!(
        h.view.link_at_caret().expect("on the link").url,
        "https://x.y"
    );
    let mut h = Harness::new("");
    h.view.insert_image("alt", "p.png");
    assert_eq!(h.text(), "![alt](p.png)");
    let mut h = Harness::new("");
    h.view.paste_code("let a = 1;");
    assert!(h.text().contains("```"), "{}", h.text());
    let mut h = Harness::new("");
    h.view.paste_markdown("- a\n- b");
    assert_eq!(h.text(), "- a\n- b");
}

#[test]
fn the_clipboard_commands_copy_cut_and_paste_source_text() {
    let mut h = Harness::new("hello world");
    let clip = clipboard(&mut h);
    h.key(KEY_A, ctrl());
    assert_eq!(h.selection(), Selection::new(0, 11));
    h.key(KEY_C, ctrl());
    assert_eq!(clip.borrow().text(), Ok(Some("hello world".to_owned())));
    h.key(KEY_X, ctrl());
    assert_eq!(h.text(), "");
    h.key(KEY_V, ctrl());
    assert_eq!(h.text(), "hello world");
    // A URL pasted over a selection becomes a link; Ctrl+Shift+V pastes plain.
    h.view.set_selection(Selection::new(0, 5));
    clip.borrow_mut()
        .set(ClipboardContent::from_text("https://x.y"))
        .expect("memory clipboard");
    h.key(KEY_V, ctrl());
    assert_eq!(h.text(), "[hello](https://x.y) world");
    h.view.set_selection(Selection::new(0, 1));
    h.key(KEY_V, ctrl() | Modifiers::SHIFT);
    assert_eq!(h.text(), "https://x.yhello](https://x.y) world");
    // Nothing selected: copy does nothing.
    h.set_caret(0);
    assert!(!h.view.execute(CommandId::EditCopy));
}
