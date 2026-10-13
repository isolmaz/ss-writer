//! Editing through the view: typing, deletion, Enter/Tab behaviors, navigation, undo grouping,
//! commands and the clipboard. Every assertion is on the document text or the selection after
//! the event, i.e. on what the `ssw_core::ops` produced.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use editor_common::{Harness, KEY_A, KEY_C, KEY_V, KEY_X, KEY_Z, TOP, close};
use ssw_core::command::CommandId;
use ssw_core::edit::Selection;
use ssw_core::markdown::ast::{Alignment, BlockKind};
use ssw_core::ops::format::InlineStyle;
use ssw_core::settings::ListIndent;
use ssw_core::text::TextRange;
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::editor::{DocumentKind, EditorEvent, EditorStrings, EditorView};
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::input::{
    Cursor, Event, ImeEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, WheelEvent,
    vk,
};
use ssw_ui::services::{ClipboardContent, MemoryClipboard, TimerId};
use ssw_ui::widget::{LayoutCx, PaintCx, Widget};
use ssw_ui::widgets::{Control, SharedClipboard};

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
fn tab_in_an_item_that_cannot_indent_inserts_nothing() {
    // The first item has no sibling to nest under; a tab character in its text is no indent.
    let mut h = Harness::new("- ab\n- c");
    h.set_caret(3);
    h.press(vk::TAB);
    assert_eq!(h.text(), "- ab\n- c");
    assert_eq!(h.selection(), Selection::caret(3));
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
    let mut h = Harness::new("");
    h.config(|config| config.document_kind = DocumentKind::PlainText);
    h.view.insert_date("1. March 2026");
    assert_eq!(h.text(), "1. March 2026", "no escape in plain text");
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

// ----- plain-text documents -------------------------------------------------------------------

fn plain(text: &str) -> Harness {
    let mut h = Harness::new(text);
    h.config(|config| config.document_kind = DocumentKind::PlainText);
    h
}

#[test]
fn plain_text_documents_delete_by_character_word_and_selection() {
    let mut h = plain("one two three");
    h.set_caret(3);
    h.press(vk::DELETE);
    assert_eq!(h.text(), "onetwo three");
    h.key(vk::DELETE, ctrl());
    assert_eq!(
        h.text(),
        "onethree",
        "Ctrl+Delete removes up to the next word"
    );
    h.set_caret(8);
    h.key(vk::BACK, ctrl());
    assert_eq!(
        h.text(),
        "",
        "Ctrl+Backspace removes the word before the caret"
    );

    let mut h = plain("one two three");
    h.view.set_selection(Selection::new(0, 3));
    h.press(vk::BACK);
    assert_eq!(h.text(), " two three", "a selection is deleted whole");

    // At the edges of the text there is nothing to delete.
    let mut h = plain("ab");
    h.set_caret(2);
    h.press(vk::DELETE);
    assert_eq!(h.text(), "ab");
    assert_eq!(h.selection(), Selection::caret(2));
    h.set_caret(0);
    h.press(vk::BACK);
    assert_eq!(h.text(), "ab");
    assert_eq!(h.selection(), Selection::caret(0));
}

#[test]
fn plain_text_tab_follows_the_indent_setting_and_shift_tab_strips_it() {
    for (indent, unit) in [
        (ListIndent::Two, "  "),
        (ListIndent::Four, "    "),
        (ListIndent::Tab, "\t"),
    ] {
        let mut h = plain("x");
        h.config(|config| config.list_indent = indent);
        h.press(vk::TAB);
        assert_eq!(h.text(), format!("{unit}x"), "{indent:?}");
        h.key(vk::TAB, Modifiers::SHIFT);
        assert_eq!(h.text(), "x", "{indent:?}: Shift+Tab removes one indent");
    }
    // Fewer spaces than the unit: only those go.
    let mut h = plain("  x");
    h.config(|config| config.list_indent = ListIndent::Four);
    h.key(vk::TAB, Modifiers::SHIFT);
    assert_eq!(h.text(), "x");
    // Nothing to remove: the key is consumed and the text stays.
    assert!(h.key(vk::TAB, Modifiers::SHIFT).handled);
    assert_eq!(h.text(), "x");
}

#[test]
fn an_ime_composition_replaces_the_selection_of_a_plain_text_document() {
    let mut h = plain("hello world");
    h.view.set_selection(Selection::new(0, 5));
    h.ime(ImeEvent::Update {
        text: "か".into(),
        caret: 3,
        attrs: Vec::new(),
    });
    assert_eq!(
        h.text(),
        " world",
        "the selected text made room for the composition"
    );
    h.ime(ImeEvent::Commit("か".into()));
    assert_eq!(h.text(), "か world");
}

// ----- keys and timers ------------------------------------------------------------------------

#[test]
fn timers_of_other_widgets_are_not_ours() {
    let mut h = Harness::new("abc");
    assert!(!h.event(Event::Timer(TimerId::new(u64::MAX))).handled);
}

#[test]
fn word_back_and_the_line_edges_extend_with_shift() {
    let mut h = Harness::new("one two three");
    h.set_caret(13);
    h.key(vk::LEFT, ctrl());
    assert_eq!(h.selection(), Selection::caret(8), "start of the last word");
    h.key(vk::HOME, Modifiers::SHIFT);
    assert_eq!(h.selection(), Selection::new(8, 0));
    h.key(vk::END, Modifiers::SHIFT);
    assert_eq!(h.selection(), Selection::new(8, 13));
}

// ----- folded text ----------------------------------------------------------------------------

fn folded() -> Harness {
    let mut h = Harness::new("# H\n\nbody\n");
    // Everything after the heading line is inside its folded section.
    h.view.set_folds(vec![TextRange::new(4, 10)], vec![0]);
    h.arrange();
    h
}

#[test]
fn a_caret_inside_a_folded_section_stays_put_and_scrolls_nothing() {
    let mut h = folded();
    h.view.set_selection(Selection::caret(6));
    for key in [vk::DOWN, vk::HOME] {
        assert!(h.press(key).handled);
        assert_eq!(
            h.selection(),
            Selection::caret(6),
            "no caret rectangle to move from"
        );
    }
    h.view.go_to(Selection::caret(7));
    h.arrange();
    assert_eq!(h.view.scroll_y(), 0.0, "nothing to scroll to");
}

#[test]
fn pointing_below_a_document_that_ends_in_a_fold_places_no_caret() {
    let mut h = folded();
    h.set_caret(1);
    let below = Point::new(200.0, 190.0);
    let result = h.pointer(PointerEventKind::Down, below, 1, Modifiers::NONE);
    assert!(result.handled && result.request_focus);
    assert_eq!(h.selection(), Selection::caret(1));
    assert_eq!(
        h.view.offset_at(below),
        None,
        "the block under the point is folded away"
    );
    h.pointer(PointerEventKind::Up, below, 0, Modifiers::NONE);

    let right = h.event(Event::Pointer(PointerEvent::mouse(
        PointerEventKind::Down,
        Some(PointerButton::Right),
        below,
    )));
    assert!(!right.handled, "no context menu below the text");

    // A drag that runs below the text keeps its selection.
    h.pointer(
        PointerEventKind::Down,
        h.first_line_point(1),
        1,
        Modifiers::NONE,
    );
    h.pointer(PointerEventKind::Move, below, 0, Modifiers::NONE);
    assert_eq!(h.selection(), Selection::caret(1));
}

#[test]
fn a_view_that_was_never_laid_out_has_nothing_visible_and_estimates_folds_as_empty() {
    let mut h = Harness::new("# H\n\nbody\n");
    h.view = EditorView::new(h.doc.clone(), h.clock.clone());
    assert!(h.view.visible_range().is_none());
    h.view.set_folds(vec![TextRange::new(4, 10)], vec![0]);
    h.arrange();
    assert!(h.view.visible_range().is_some());
}

// ----- accessors and stale state --------------------------------------------------------------

#[test]
fn the_view_describes_itself_and_takes_new_strings() {
    let mut h = Harness::new("one two\n");
    let printed = format!("{:?}", h.view);
    assert!(printed.contains("zoom_percent: 100"), "{printed}");
    assert!(printed.contains("typewriter: false"), "{printed}");

    let strings = EditorStrings {
        name: "Document".to_owned(),
        ..EditorStrings::default()
    };
    h.view.set_strings(strings);
    assert_eq!(h.access().name, "Document");

    h.view.set_selection(Selection::new(0, 3));
    assert_eq!(h.view.selected_text(), "one");
    // Switching a mode to the state it has changes nothing.
    h.view.set_typewriter(false);
    assert!(!h.view.typewriter());
    h.view.set_focus_mode(true);
    assert!(h.view.focus_mode());
}

#[test]
fn rectangles_wait_for_the_layout_of_an_edited_document() {
    let mut h = Harness::new("one\n");
    assert!(h.view.rect_at(1).is_some());
    h.type_text("x");
    assert!(
        h.view.rect_at(1).is_none(),
        "the layout is older than the text"
    );
    h.arrange();
    assert!(h.view.rect_at(1).is_some());
}

#[test]
fn dropping_nothing_or_removing_nothing_is_refused() {
    let mut h = Harness::new("one two\n");
    assert!(!h.view.drop_text(h.first_line_point(2), ""));
    assert!(!h.view.remove_range(TextRange::new(2, 2)));
    assert!(!h.view.remove_range(TextRange::new(0, 999)));
    assert_eq!(h.text(), "one two\n");
}

#[test]
fn removing_an_item_of_a_numbered_list_renumbers_the_rest() {
    let mut h = Harness::new("1. a\n2. b\n3. c\n");
    assert!(h.view.remove_range(TextRange::new(5, 10)));
    assert_eq!(h.text(), "1. a\n2. c\n");
    assert_eq!(h.view.selection(), Selection::caret(5));
}

#[test]
fn an_unbounded_space_measures_the_column_and_a_default_height() {
    let mut h = Harness::new("a\n");
    let column = h.view.config().column_width();
    let id = h.view.id();
    let mut cx = LayoutCx::new(id, &h.theme, 1.0, &h.engine, &mut h.focus, &mut h.requests);
    let size = h.view.measure(
        &mut cx,
        ssw_ui::geometry::Size::new(f32::INFINITY, f32::INFINITY),
    );
    assert!(close(size.width, column + 48.0), "{size:?}");
    assert!(close(size.height, 600.0), "{size:?}");
}

// ----- painting -------------------------------------------------------------------------------

fn paint_damaged(h: &Harness, damage: Rect) -> RecordingCanvas {
    let mut canvas = RecordingCanvas::new();
    let cx = PaintCx::new(
        h.view.id(),
        &h.theme,
        1.0,
        &h.engine,
        &h.focus,
        &h.capture,
        Some(damage),
    );
    h.view.paint(&cx, &mut canvas);
    assert!(canvas.is_balanced());
    canvas
}

#[test]
fn only_blocks_in_the_damaged_area_are_painted() {
    let h = Harness::new("a\n\nb\n\nc\n");
    assert!(
        paint_damaged(&h, Rect::new(1000.0, 1000.0, 10.0, 10.0))
            .recorded()
            .is_empty(),
        "damage elsewhere paints nothing"
    );
    let top = paint_damaged(&h, Rect::new(0.0, 0.0, 400.0, 40.0));
    assert_eq!(
        top.texts(),
        ["a"],
        "the blocks below the damage are skipped"
    );
}

fn highlight_fills(canvas: &RecordingCanvas) -> Vec<ssw_ui::geometry::Color> {
    canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::FillRoundedRect { color, .. } => Some(*color),
            _ => None,
        })
        .collect()
}

#[test]
fn search_matches_are_highlighted_and_the_current_one_stands_out() {
    let mut h = Harness::new("ab\n\nab\n");
    let (current, other) = {
        let palette = &h.theme.tokens.highlight;
        (palette.orange.background, palette.yellow.background)
    };
    h.view
        .set_search_highlights(vec![TextRange::new(0, 2), TextRange::new(4, 6)], Some(0));
    assert_eq!(highlight_fills(&h.paint()), [current, other]);
    // The same request again changes nothing.
    h.view
        .set_search_highlights(vec![TextRange::new(0, 2), TextRange::new(4, 6)], Some(0));
    assert_eq!(highlight_fills(&h.paint()), [current, other]);
    // A current index past the matches means none is current.
    h.view
        .set_search_highlights(vec![TextRange::new(0, 2), TextRange::new(4, 6)], Some(9));
    assert_eq!(highlight_fills(&h.paint()), [other, other]);
}

// ----- pointer and wheel ----------------------------------------------------------------------

fn wheel_at(h: &mut Harness, position: Point, delta: Point, mods: Modifiers) -> bool {
    h.event(Event::Wheel(WheelEvent {
        delta,
        precise: true,
        position,
        mods,
    }))
    .handled
}

#[test]
fn wheels_outside_the_view_and_zero_zoom_wheels_do_nothing() {
    let mut h = Harness::new("abc\n");
    h.events();
    let outside = Point::new(500.0, 500.0);
    assert!(!wheel_at(
        &mut h,
        outside,
        Point::new(0.0, -1.0),
        Modifiers::NONE
    ));
    assert!(h.wheel(0.0, false, Modifiers::CTRL).handled);
    assert_eq!(h.view.zoom_percent(), 100);
    assert!(h.events().is_empty(), "no zoom event without a zoom step");
}

#[test]
fn shift_wheel_scrolls_unwrapped_text_sideways() {
    let mut h = plain(&format!("{}\n", "x".repeat(100)));
    h.config(|config| config.wrap = false);
    let text_x = |h: &Harness| {
        h.paint()
            .recorded()
            .iter()
            .find_map(|entry| match &entry.op {
                DrawOp::Text { .. } => entry.bounds.map(|bounds| bounds.x),
                _ => None,
            })
            .expect("the line is drawn")
    };
    assert!(close(text_x(&h), 24.0));
    assert!(wheel_at(
        &mut h,
        Point::new(100.0, 100.0),
        Point::new(0.0, -20.0),
        Modifiers::SHIFT
    ));
    h.arrange();
    assert!(close(text_x(&h), 4.0), "scrolled 20 DIPs: {}", text_x(&h));
}

#[test]
fn pointer_events_the_view_does_not_own_are_ignored() {
    let mut h = Harness::new("a\n\nb\n");
    let inside = h.first_line_point(0);
    // Leaving after hovering, and moving out of the view, end the hover.
    h.pointer(PointerEventKind::Move, inside, 0, Modifiers::NONE);
    h.requests.take_invalidation();
    assert!(
        !h.pointer(PointerEventKind::Leave, inside, 0, Modifiers::NONE)
            .handled
    );
    assert!(
        !h.requests.take_invalidation().is_clean(),
        "the hover highlight is repainted away"
    );
    h.pointer(PointerEventKind::Move, inside, 0, Modifiers::NONE);
    h.requests.take_invalidation();
    let outside = Point::new(500.0, 500.0);
    assert!(
        !h.pointer(PointerEventKind::Move, outside, 0, Modifiers::NONE)
            .handled
    );
    assert!(!h.requests.take_invalidation().is_clean());
    // With nothing hovered, leaving or moving out of the view repaints nothing.
    h.pointer(PointerEventKind::Leave, inside, 0, Modifiers::NONE);
    assert!(h.requests.take_invalidation().is_clean());
    h.pointer(PointerEventKind::Move, outside, 0, Modifiers::NONE);
    assert!(h.requests.take_invalidation().is_clean());
    // A press outside, and buttons other than the left and right ones.
    assert!(
        !h.pointer(PointerEventKind::Down, outside, 1, Modifiers::NONE)
            .handled
    );
    let up = PointerEvent::mouse(PointerEventKind::Up, Some(PointerButton::Right), inside);
    assert!(!h.event(Event::Pointer(up)).handled);
    let right_outside =
        PointerEvent::mouse(PointerEventKind::Down, Some(PointerButton::Right), outside);
    assert!(!h.event(Event::Pointer(right_outside)).handled);
}

#[test]
fn the_unwrapped_view_scrolls_back_to_a_caret_left_of_it() {
    let mut h = plain(&format!("{}\n", "x".repeat(100)));
    h.config(|config| config.wrap = false);
    h.press(vk::END);
    h.arrange();
    h.press(vk::HOME);
    h.arrange();
    let text_x = |h: &Harness| {
        h.paint()
            .recorded()
            .iter()
            .find_map(|entry| match &entry.op {
                DrawOp::Text { .. } => entry.bounds.map(|bounds| bounds.x),
                _ => None,
            })
            .expect("the line is drawn")
    };
    assert!(
        close(text_x(&h), 24.0),
        "the line starts at the column again: {}",
        text_x(&h)
    );
    // A caret that stays inside the visible part leaves the view where it is.
    h.press(vk::RIGHT);
    h.press(vk::RIGHT);
    h.arrange();
    assert!(
        close(text_x(&h), 24.0),
        "the visible caret needs no scrolling: {}",
        text_x(&h)
    );
}

#[test]
fn the_cursor_follows_gutter_widgets_and_ctrl_over_links() {
    let mut h = Harness::new("# Title\n\nbody\n");
    h.set_caret(12);
    let at = |x: f32| Point::new(x, TOP + 26.0);
    // The fold chevron is a button, the rest of the gutter is not.
    h.pointer(
        PointerEventKind::Move,
        at(h.left - 12.0),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
    // Between the chevron and the column: still the gutter (the column starts at x = 24).
    h.pointer(PointerEventKind::Move, at(h.left - 1.0), 0, Modifiers::NONE);
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Arrow));

    let mut h = Harness::new("see [link](http://x.test)\n");
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(5),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::IBeam));
    h.pointer(PointerEventKind::Move, h.first_line_point(5), 0, ctrl());
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
}

#[test]
fn the_cursor_is_an_arrow_where_there_is_no_text() {
    let mut h = Harness::new("see\n");
    let cursor_at = |h: &mut Harness, x: f32, y: f32| {
        h.pointer(PointerEventKind::Move, Point::new(x, y), 0, Modifiers::NONE);
        h.requests.take_cursor()
    };
    let (left, line) = (h.left, TOP + 14.0);
    assert_eq!(cursor_at(&mut h, left + 100.0, line), Some(Cursor::IBeam));
    assert_eq!(
        cursor_at(&mut h, left + 100.0, TOP / 2.0),
        Some(Cursor::Arrow)
    );
    assert_eq!(
        cursor_at(&mut h, left + 100.0, TOP + 100.0),
        Some(Cursor::Arrow)
    );
    assert_eq!(
        cursor_at(&mut h, left + 352.0 + 4.0, line),
        Some(Cursor::Arrow)
    );
}

#[test]
fn a_footnote_reference_with_a_definition_is_a_hand_and_one_without_is_text() {
    let mut h = Harness::new("a[^1]\n\n[^1]: note\n");
    h.set_caret(18);
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(1),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(1),
        0,
        Modifiers::SHIFT,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::IBeam));

    let mut h = Harness::new("a[^2]\n\nb\n");
    h.set_caret(8);
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(1),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::IBeam));
}

#[test]
fn dragging_by_words_and_paragraphs_extends_by_whole_units() {
    let mut h = Harness::new("one two three\n\nfour five\n");
    h.pointer(
        PointerEventKind::Down,
        h.first_line_point(5),
        2,
        Modifiers::NONE,
    );
    assert_eq!(
        h.selection(),
        Selection::new(4, 7),
        "the double-clicked word"
    );
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(10),
        0,
        Modifiers::NONE,
    );
    assert_eq!(
        h.selection(),
        Selection::new(4, 13),
        "extended to the end of a word"
    );
    // Moving within the same word selects the same text and reports nothing.
    h.events();
    h.pointer(
        PointerEventKind::Move,
        h.first_line_point(11),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::new(4, 13));
    assert!(h.events().is_empty());
    h.pointer(
        PointerEventKind::Up,
        h.first_line_point(10),
        0,
        Modifiers::NONE,
    );

    h.pointer(
        PointerEventKind::Down,
        h.first_line_point(1),
        3,
        Modifiers::NONE,
    );
    let below = Point::new(h.left + 1.0, TOP + 28.0 + 8.0 + 14.0);
    h.pointer(PointerEventKind::Move, below, 0, Modifiers::NONE);
    let selection = h.selection();
    assert_eq!(selection.anchor, 0);
    assert!(
        selection.head >= 24,
        "through the second paragraph: {selection:?}"
    );
}

#[test]
fn the_autoscroll_follows_the_pointer_while_it_stays_at_the_edge() {
    let text: String = (0..100).map(|i| format!("para {i}\n\n")).collect();
    let mut h = Harness::new(&text);
    h.pointer(
        PointerEventKind::Down,
        h.first_line_point(1),
        1,
        Modifiers::NONE,
    );
    h.pointer(
        PointerEventKind::Move,
        Point::new(100.0, 230.0),
        0,
        Modifiers::NONE,
    );
    let timers = h.requests.timers().len();
    h.pointer(
        PointerEventKind::Move,
        Point::new(120.0, 260.0),
        0,
        Modifiers::NONE,
    );
    assert_eq!(
        h.requests.timers().len(),
        timers,
        "the running timer is reused"
    );
}

// ----- commands -------------------------------------------------------------------------------

#[test]
fn inline_format_commands_wrap_the_selection_in_their_markers() {
    let mut italic = Harness::new("word");
    italic.view.set_selection(Selection::new(0, 4));
    assert!(italic.view.execute(CommandId::FormatItalic));
    assert!(italic.view.style_active(InlineStyle::Italic));

    for (command, expected) in [
        (CommandId::FormatUnderline, "~word~"),
        (CommandId::FormatStrikethrough, "~~word~~"),
        (CommandId::FormatHighlight, "==word=="),
        (CommandId::FormatHighlightRed, "=={red}word=="),
        (CommandId::FormatHighlightOrange, "=={orange}word=="),
        (CommandId::FormatHighlightYellow, "=={yellow}word=="),
        (CommandId::FormatHighlightGreen, "=={green}word=="),
        (CommandId::FormatHighlightBlue, "=={blue}word=="),
        (CommandId::FormatHighlightPurple, "=={purple}word=="),
        (CommandId::FormatInlineCode, "`word`"),
        (CommandId::FormatMath, "$word$"),
    ] {
        let mut h = Harness::new("word");
        h.view.set_selection(Selection::new(0, 4));
        assert!(h.view.execute(command), "{command:?}");
        assert_eq!(h.text(), expected, "{command:?}");
    }
}

#[test]
fn block_format_commands_insert_and_convert_blocks() {
    let has = |h: &Harness, wanted: fn(&BlockKind) -> bool| {
        h.doc
            .borrow()
            .ast()
            .blocks
            .iter()
            .any(|block| wanted(&block.kind))
    };
    let mut h = Harness::new("text");
    h.set_caret(4);
    assert!(h.view.execute(CommandId::FormatDivider));
    assert!(
        has(&h, |kind| matches!(kind, BlockKind::ThematicBreak)),
        "{:?}",
        h.text()
    );
    let mut h = Harness::new("text");
    h.set_caret(4);
    assert!(h.view.execute(CommandId::FormatPageBreak));
    assert!(
        has(&h, |kind| matches!(kind, BlockKind::PageBreak)),
        "{:?}",
        h.text()
    );
    let mut h = Harness::new("text");
    h.set_caret(4);
    assert!(h.view.execute(CommandId::FormatTable));
    assert!(
        has(&h, |kind| matches!(kind, BlockKind::Table(_))),
        "{:?}",
        h.text()
    );
    assert_eq!(
        h.selection(),
        Selection::caret(8),
        "in the first header cell"
    );
    let mut h = Harness::new("text");
    h.set_caret(4);
    assert!(h.view.execute(CommandId::FormatFootnote));
    assert_eq!(h.text().matches("[^1]").count(), 2, "{:?}", h.text());

    let mut h = Harness::new("text");
    h.set_caret(2);
    assert!(h.view.execute(CommandId::FormatNumberedList));
    assert_eq!(h.text(), "1. text");
    let mut h = Harness::new("text");
    h.set_caret(2);
    assert!(h.view.execute(CommandId::FormatCallout));
    assert!(h.text().starts_with("> [!NOTE]"), "{:?}", h.text());
    let mut h = Harness::new("text");
    h.set_caret(2);
    assert!(h.view.execute(CommandId::FormatMathBlock));
    assert!(h.text().contains("$$"), "{:?}", h.text());
}

#[test]
fn table_commands_edit_rows_columns_and_alignment() {
    const SAMPLE: &str = "| Name | Qty |\n|:-----|----:|\n| Apple | 3 |\n| Pear | 12 |\n";
    let run = |command: CommandId| {
        let mut h = Harness::new(SAMPLE);
        h.set_caret(h.text().find("Apple").expect("Apple"));
        assert!(h.view.execute(command), "{command:?}");
        let doc = h.doc.borrow();
        let BlockKind::Table(table) = doc.ast().blocks[0].kind.clone() else {
            panic!("{command:?} left no table: {:?}", doc.text());
        };
        table
    };
    assert_eq!(run(CommandId::TableInsertRowBelow).rows.len(), 3);
    assert_eq!(run(CommandId::TableDeleteRow).rows.len(), 1);
    assert_eq!(run(CommandId::TableInsertColumnLeft).header.cells.len(), 3);
    assert_eq!(run(CommandId::TableInsertColumnRight).header.cells.len(), 3);
    assert_eq!(run(CommandId::TableDeleteColumn).header.cells.len(), 1);
    for (command, alignment) in [
        (CommandId::TableAlignLeft, Alignment::Left),
        (CommandId::TableAlignCenter, Alignment::Center),
        (CommandId::TableAlignRight, Alignment::Right),
    ] {
        assert_eq!(run(command).alignments[0], alignment, "{command:?}");
    }
}

#[test]
fn the_mode_commands_toggle_and_report_the_modes() {
    let mut h = Harness::new("a\n");
    h.events();
    assert!(h.view.execute(CommandId::ViewTypewriterMode));
    assert!(h.view.typewriter());
    assert_eq!(
        h.events(),
        [EditorEvent::ModesChanged {
            typewriter: true,
            focus: false
        }]
    );
    assert!(h.view.execute(CommandId::ViewFocusMode));
    assert!(h.view.focus_mode());
    assert_eq!(
        h.events(),
        [EditorEvent::ModesChanged {
            typewriter: true,
            focus: true
        }]
    );
}

#[test]
fn pasting_needs_a_clipboard_with_text() {
    let mut h = Harness::new("abc");
    assert!(!h.view.execute(CommandId::EditPaste), "no clipboard");
    let _clip = clipboard(&mut h);
    assert!(!h.view.execute(CommandId::EditPaste), "an empty clipboard");
    assert_eq!(h.text(), "abc");
}
