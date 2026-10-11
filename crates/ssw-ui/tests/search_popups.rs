//! Editor popups: the B I U pill and the formatting state at the caret, the heading level menu,
//! the selection toolbar, the link edit popup and the wikilink completion.

mod search_common;

use search_common::{Host, Shortcuts, child_names, clipboard, covered, localizer, node_named};
use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::edit::Selection;
use ssw_core::links::RelPath;
use ssw_core::toc::Toc;
use ssw_ui::access::{AccessAction, Role, ToggleState};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, Modifiers, PointerEventKind, WheelEvent, vk};
use ssw_ui::shell::popups::{
    CommandMenu, CommandMenuEvent, FormatPill, FormatPillEvent, FormatState, HeadingLevelMenu,
    LinkEditEvent, LinkEditPopup, SelectionToolbar, ToolbarEvent, WikilinkAccept,
    WikilinkCompletion, WikilinkEvent, format_state_at,
};
use ssw_ui::widget::{Widget, WidgetId};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;
use ssw_ui::widgets::menu::{MenuEntry, MenuItem};

const WINDOW: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);
/// Virtual-key code of the letter A (`ssw_ui::input::vk` only names the non-letter keys).
const KEY_A: u16 = 0x41;

// ------------------------------------------------------------------------ format state

fn state_of(text: &str, from: usize, to: usize) -> FormatState {
    let mut doc = Document::new(text);
    doc.set_selections(Selection::new(from, to), &[]);
    format_state_at(&doc)
}

#[test]
fn the_format_state_follows_the_elements_around_the_caret() {
    let text = "x *it* y ~un~ z **bold _both_ w**";
    let inside = |needle: &str, shift: usize| text.find(needle).expect("needle") + shift;
    let it = inside("it", 1);
    assert_eq!(
        state_of(text, it, it),
        FormatState {
            italic: true,
            ..FormatState::default()
        }
    );
    let un = inside("un", 1);
    assert_eq!(
        state_of(text, un, un),
        FormatState {
            underline: true,
            ..FormatState::default()
        }
    );
    let both = inside("both", 2);
    assert_eq!(
        state_of(text, both, both),
        FormatState {
            bold: true,
            italic: true,
            underline: false
        }
    );
    let w = inside(" w**", 1);
    assert_eq!(
        state_of(text, w, w),
        FormatState {
            bold: true,
            ..FormatState::default()
        }
    );
    assert_eq!(state_of(text, 0, 0), FormatState::default());
    // A selection counts when it lies inside the content; one reaching into the markers does not.
    let bold = "a **bold** b";
    assert!(state_of(bold, 4, 8).bold);
    assert!(state_of(bold, 5, 7).bold);
    assert!(!state_of(bold, 2, 8).bold);
    // The edges of the content count, the markers do not.
    assert!(state_of(bold, 4, 4).bold && state_of(bold, 8, 8).bold);
    assert!(!state_of(bold, 2, 2).bold && !state_of(bold, 10, 10).bold);
    // Elements inside list items.
    let list = "- **item**";
    assert!(state_of(list, 5, 5).bold);
    assert_eq!(state_of("", 0, 0), FormatState::default());
}

// ------------------------------------------------------------------------ pill

fn pill() -> (Host, FormatPill) {
    let mut host = Host::new();
    let mut pill = FormatPill::new(&localizer()).with_motion(Motion::reduced());
    host.layout(&mut pill, Rect::new(0.0, 0.0, 800.0, 40.0));
    (host, pill)
}

#[test]
fn the_pill_sits_at_the_right_of_its_slot_and_reflects_the_caret_state() {
    let (host, mut pill) = pill();
    assert_eq!(pill.bounds(), Rect::new(704.0, 2.0, 96.0, 36.0));
    let tree = host.tree(&pill);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.role, Role::ToolBar);
    assert_eq!(root.name, "Text style");
    assert_eq!(child_names(&tree, root.id), ["Bold", "Italic", "Underline"]);
    assert_eq!(
        node_named(&tree, "Bold").state.toggle,
        Some(ToggleState::Off)
    );
    pill.set_state(FormatState {
        bold: true,
        italic: true,
        underline: false,
    });
    assert!(pill.state().bold);
    let tree = host.tree(&pill);
    assert_eq!(
        node_named(&tree, "Bold").state.toggle,
        Some(ToggleState::On)
    );
    assert_eq!(
        node_named(&tree, "Italic").state.toggle,
        Some(ToggleState::On)
    );
    assert_eq!(
        node_named(&tree, "Underline").state.toggle,
        Some(ToggleState::Off)
    );
    assert_eq!(host.paint(&pill).texts(), ["B", "I", "U"]);
}

#[test]
fn a_click_asks_for_the_command_and_waits_for_the_new_state() {
    let (mut host, mut pill) = pill();
    pill.set_state(FormatState {
        bold: true,
        ..FormatState::default()
    });
    let tree = host.tree(&pill);
    let bold = node_named(&tree, "Bold").bounds.center();
    let underline = node_named(&tree, "Underline").bounds.center();
    host.click(&mut pill, bold);
    host.click(&mut pill, underline);
    assert_eq!(
        pill.take_events(),
        vec![
            FormatPillEvent::Toggle(CommandId::FormatBold),
            FormatPillEvent::Toggle(CommandId::FormatUnderline)
        ]
    );
    // The chips still show the caret state until the host reports the new one.
    let tree = host.tree(&pill);
    assert_eq!(
        node_named(&tree, "Bold").state.toggle,
        Some(ToggleState::On)
    );
    assert_eq!(
        node_named(&tree, "Underline").state.toggle,
        Some(ToggleState::Off)
    );
    // Keyboard: the first tab stop is Bold.
    host.focus.focus_next();
    host.key(&mut pill, vk::SPACE);
    assert_eq!(
        pill.take_events(),
        vec![FormatPillEvent::Toggle(CommandId::FormatBold)]
    );
    // Clicks on the pill itself do not fall through to the editor.
    let bounds = pill.bounds();
    assert!(
        host.click(&mut pill, Point::new(bounds.x + 1.0, bounds.y + 1.0))
            .handled
    );
    assert!(!host.click(&mut pill, Point::new(10.0, 10.0)).handled);
}

#[test]
fn a_hidden_pill_draws_nothing_and_takes_no_input() {
    let (mut host, mut pill) = pill();
    assert!(pill.is_visible());
    pill.set_visible(false);
    assert!(!pill.is_visible());
    host.layout(&mut pill, Rect::new(0.0, 0.0, 800.0, 40.0));
    assert_eq!(pill.bounds(), Rect::ZERO);
    assert!(host.paint(&pill).recorded().is_empty());
    assert!(host.tree(&pill).root().expect("root").state.offscreen);
    assert!(!host.click(&mut pill, Point::new(750.0, 20.0)).handled);
    assert!(pill.take_events().is_empty());
    pill.set_visible(true);
    host.layout(&mut pill, Rect::new(0.0, 0.0, 800.0, 40.0));
    assert!(!host.paint(&pill).recorded().is_empty());
}

// ------------------------------------------------------------------------ heading menu

fn open_heading_menu(level: Option<u8>) -> (Host, HeadingLevelMenu) {
    let mut host = Host::new();
    let mut menu = HeadingLevelMenu::new(localizer(), &Shortcuts);
    let mut cx = host.cx(menu.id());
    menu.open_at(&mut cx, Point::new(100.0, 100.0), level);
    (host, menu)
}

#[test]
fn the_heading_menu_checks_the_current_level_and_shows_shortcuts() {
    let (host, menu) = open_heading_menu(Some(2));
    assert!(menu.is_open());
    let tree = host.tree(&menu);
    let root = tree.root().expect("root").clone();
    assert_eq!(
        child_names(&tree, root.id),
        [
            "Paragraph",
            "Heading 1",
            "Heading 2",
            "Heading 3",
            "Heading 4",
            "Heading 5",
            "Heading 6"
        ]
    );
    assert_eq!(
        node_named(&tree, "Heading 2").state.toggle,
        Some(ToggleState::On)
    );
    assert_eq!(
        node_named(&tree, "Heading 1").state.toggle,
        Some(ToggleState::Off)
    );
    assert_eq!(node_named(&tree, "Heading 1").description, "Ctrl+1");
    let (host, menu) = open_heading_menu(None);
    let tree = host.tree(&menu);
    assert_eq!(
        node_named(&tree, "Paragraph").state.toggle,
        Some(ToggleState::On)
    );
    assert!(host.paint(&menu).texts().contains(&"Heading 3"));
}

#[test]
fn the_heading_menu_runs_the_chosen_command() {
    let (mut host, mut menu) = open_heading_menu(Some(1));
    for _ in 0..4 {
        host.key(&mut menu, vk::DOWN);
    }
    assert_eq!(menu.take_events(), vec![]);
    host.key(&mut menu, vk::RETURN);
    assert_eq!(
        menu.take_events(),
        vec![CommandMenuEvent::Command(CommandId::FormatHeading3)]
    );
    assert!(!menu.is_open());
    // Escape dismisses.
    let mut cx = host.cx(menu.id());
    menu.open_at(&mut cx, Point::new(100.0, 100.0), None);
    host.key(&mut menu, vk::ESCAPE);
    assert_eq!(menu.take_events(), vec![CommandMenuEvent::Dismissed]);
    // Assistive technology invokes an item.
    let mut cx = host.cx(menu.id());
    menu.open_at(&mut cx, Point::new(100.0, 100.0), Some(6));
    let tree = host.tree(&menu);
    let item = node_named(&tree, "Heading 5").id;
    let mut cx = host.cx(menu.id());
    assert!(menu.perform_item(&mut cx, item, &AccessAction::Invoke));
    assert_eq!(
        menu.take_events(),
        vec![CommandMenuEvent::Command(CommandId::FormatHeading5)]
    );
    let mut cx = host.cx(menu.id());
    menu.close(&mut cx);
    assert!(!host.paint(&menu).texts().contains(&"Heading 5"));
}

// ------------------------------------------------------------------------ toolbar

fn toolbar_at(anchor: Rect) -> (Host, SelectionToolbar) {
    let mut host = Host::new();
    let mut toolbar = SelectionToolbar::new(localizer(), &Shortcuts).with_motion(Motion::reduced());
    toolbar.show(anchor);
    host.layout(&mut toolbar, WINDOW);
    (host, toolbar)
}

fn toolbar() -> (Host, SelectionToolbar) {
    toolbar_at(Rect::new(300.0, 200.0, 100.0, 20.0))
}

fn click_named(host: &mut Host, toolbar: &mut SelectionToolbar, name: &str) {
    let tree = host.tree(toolbar);
    let at = node_named(&tree, name).bounds.center();
    host.click(toolbar, at);
}

#[test]
fn the_toolbar_floats_above_the_selection_with_the_items_of_the_spec() {
    let (host, toolbar) = toolbar();
    assert_eq!(toolbar.bounds(), Rect::new(168.0, 154.0, 364.0, 40.0));
    let tree = host.tree(&toolbar);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.role, Role::ToolBar);
    assert_eq!(
        child_names(&tree, root.id),
        [
            "Heading",
            "Toggle task",
            "List",
            "Bold",
            "Italic",
            "Highlight color",
            "Link",
            "Table",
            "Image",
            "More formatting"
        ]
    );
    assert_eq!(node_named(&tree, "Heading").state.expanded, Some(false));
    assert_eq!(node_named(&tree, "Bold").state.expanded, None);
    let icons = host
        .paint(&toolbar)
        .ops()
        .filter(|op| matches!(op, DrawOp::Icon { .. }))
        .count();
    // Ten buttons and a chevron on the three dropdowns.
    assert_eq!(icons, 13);
    // Placement: flips below near the top, stays inside at the edges.
    let (_host, near_top) = toolbar_at(Rect::new(300.0, 20.0, 100.0, 20.0));
    assert_eq!(near_top.bounds().y, 46.0);
    let (_host, left) = toolbar_at(Rect::new(0.0, 200.0, 20.0, 20.0));
    assert_eq!(left.bounds().x, 8.0);
    let (_host, right) = toolbar_at(Rect::new(780.0, 200.0, 20.0, 20.0));
    assert_eq!(right.bounds().right(), 792.0);
}

#[test]
fn command_buttons_report_their_command() {
    let (mut host, mut toolbar) = toolbar();
    click_named(&mut host, &mut toolbar, "Bold");
    click_named(&mut host, &mut toolbar, "Toggle task");
    click_named(&mut host, &mut toolbar, "Link");
    assert_eq!(
        toolbar.take_events(),
        vec![
            ToolbarEvent::Command(CommandId::FormatBold),
            ToolbarEvent::Command(CommandId::FormatTaskToggle),
            ToolbarEvent::Command(CommandId::FormatLink)
        ]
    );
    assert!(toolbar.is_visible());
    click_named(&mut host, &mut toolbar, "Table");
    click_named(&mut host, &mut toolbar, "Image");
    click_named(&mut host, &mut toolbar, "Italic");
    assert_eq!(toolbar.take_events().len(), 3);
}

fn menu_items(host: &Host, toolbar: &SelectionToolbar) -> Vec<String> {
    let tree = host.tree(toolbar);
    let root = tree.root().expect("root");
    let menu = tree
        .children(root.id)
        .find(|node| node.role == Role::Menu)
        .expect("an open menu");
    tree.children(menu.id)
        .map(|node| node.name.clone())
        .collect()
}

#[test]
fn the_dropdowns_open_menus_of_commands() {
    let (mut host, mut toolbar) = toolbar();
    click_named(&mut host, &mut toolbar, "Heading");
    assert!(toolbar.is_menu_open());
    assert_eq!(
        menu_items(&host, &toolbar),
        [
            "Paragraph",
            "Heading 1",
            "Heading 2",
            "Heading 3",
            "Heading 4",
            "Heading 5",
            "Heading 6"
        ]
    );
    assert_eq!(
        node_named(&host.tree(&toolbar), "Heading").state.expanded,
        Some(true)
    );
    host.key(&mut toolbar, vk::DOWN);
    host.key(&mut toolbar, vk::DOWN);
    host.key(&mut toolbar, vk::RETURN);
    assert_eq!(
        toolbar.take_events(),
        vec![ToolbarEvent::Command(CommandId::FormatHeading1)]
    );
    assert!(!toolbar.is_menu_open());
    // Highlight: the default and the six colors.
    click_named(&mut host, &mut toolbar, "Highlight color");
    assert_eq!(
        menu_items(&host, &toolbar),
        [
            "Highlight",
            "Red",
            "Orange",
            "Yellow",
            "Green",
            "Blue",
            "Purple"
        ]
    );
    host.key(&mut toolbar, vk::DOWN);
    host.key(&mut toolbar, vk::DOWN);
    host.key(&mut toolbar, vk::RETURN);
    assert_eq!(
        toolbar.take_events(),
        vec![ToolbarEvent::Command(CommandId::FormatHighlightRed)]
    );
    // List: three kinds.
    click_named(&mut host, &mut toolbar, "List");
    assert_eq!(
        menu_items(&host, &toolbar),
        ["Bulleted list", "Numbered list", "Task list"]
    );
    // Escape closes only the menu.
    host.key(&mut toolbar, vk::ESCAPE);
    assert!(!toolbar.is_menu_open() && toolbar.is_visible());
    assert!(toolbar.take_events().is_empty());
    // More: the overflow items.
    click_named(&mut host, &mut toolbar, "More formatting");
    assert_eq!(
        menu_items(&host, &toolbar),
        [
            "Underline",
            "Strikethrough",
            "Inline code",
            "Code block",
            "Quote",
            "Callout",
            "Inline math",
            "Divider",
            "Footnote",
            "Insert date"
        ]
    );
    let item = node_named(&host.tree(&toolbar), "Quote").id;
    let mut cx = host.cx(toolbar.id());
    assert!(toolbar.perform_child(&mut cx, item, &AccessAction::Invoke));
    assert_eq!(
        toolbar.take_events(),
        vec![ToolbarEvent::Command(CommandId::FormatQuote)]
    );
    // The open menu is painted on top of the toolbar.
    click_named(&mut host, &mut toolbar, "List");
    assert!(host.paint(&toolbar).texts().contains(&"Numbered list"));
}

#[test]
fn the_toolbar_hides_on_escape_typing_scrolling_and_outside_presses() {
    let anchor = Rect::new(300.0, 200.0, 100.0, 20.0);
    let (mut host, mut toolbar) = toolbar_at(anchor);
    assert!(host.key(&mut toolbar, vk::ESCAPE).handled);
    assert!(!toolbar.is_visible());
    assert_eq!(toolbar.take_events(), vec![ToolbarEvent::Hidden]);
    toolbar.hide();
    assert!(toolbar.take_events().is_empty());
    host.layout(&mut toolbar, WINDOW);
    assert_eq!(toolbar.bounds(), Rect::ZERO);
    assert!(host.paint(&toolbar).recorded().is_empty());
    assert!(!host.key(&mut toolbar, vk::ESCAPE).handled);
    // Typing hides it and still reaches the editor.
    toolbar.show(anchor);
    host.layout(&mut toolbar, WINDOW);
    assert!(!host.text(&mut toolbar, "a").handled);
    assert_eq!(toolbar.take_events(), vec![ToolbarEvent::Hidden]);
    // So does scrolling.
    toolbar.show(anchor);
    host.layout(&mut toolbar, WINDOW);
    let wheel = Event::Wheel(WheelEvent {
        delta: Point::new(0.0, -1.0),
        precise: false,
        position: Point::new(10.0, 10.0),
        mods: Modifiers::NONE,
    });
    assert!(!host.send(&mut toolbar, wheel).handled);
    assert_eq!(toolbar.take_events(), vec![ToolbarEvent::Hidden]);
    // A press outside hides it without consuming the press; one on the bar does not.
    toolbar.show(anchor);
    host.layout(&mut toolbar, WINDOW);
    let bounds = toolbar.bounds();
    assert!(
        host.click(&mut toolbar, Point::new(bounds.x + 1.0, bounds.y + 1.0))
            .handled
    );
    assert!(toolbar.is_visible());
    assert!(
        !host
            .pointer(&mut toolbar, PointerEventKind::Down, Point::new(5.0, 5.0))
            .handled
    );
    assert!(!toolbar.is_visible());
    assert_eq!(toolbar.take_events(), vec![ToolbarEvent::Hidden]);
    // Hidden toolbars are offscreen in the access tree and refuse actions.
    let tree = host.tree(&toolbar);
    assert!(tree.root().expect("root").state.offscreen);
    let mut cx = host.cx(toolbar.id());
    assert!(!toolbar.perform_child(&mut cx, WidgetId::allocate(), &AccessAction::Invoke));
}

#[test]
fn assistive_technology_can_press_toolbar_buttons() {
    let (mut host, mut toolbar) = toolbar();
    let bold = node_named(&host.tree(&toolbar), "Bold").id;
    let heading = node_named(&host.tree(&toolbar), "Heading").id;
    let mut cx = host.cx(toolbar.id());
    assert!(toolbar.perform_child(&mut cx, bold, &AccessAction::Invoke));
    assert_eq!(
        toolbar.take_events(),
        vec![ToolbarEvent::Command(CommandId::FormatBold)]
    );
    assert!(!toolbar.perform_child(&mut cx, WidgetId::allocate(), &AccessAction::Invoke));
    // A dropdown opens its menu.
    assert!(toolbar.perform_child(&mut cx, heading, &AccessAction::Invoke));
    assert!(toolbar.is_menu_open());
}

#[test]
fn keys_and_pointers_the_toolbar_has_no_use_for_pass_through_and_repaint_nothing() {
    let (mut host, mut toolbar) = toolbar();
    host.requests.take_invalidation();
    assert!(!host.key(&mut toolbar, KEY_A).handled);
    assert!(
        !host
            .pointer(&mut toolbar, PointerEventKind::Move, Point::new(5.0, 5.0))
            .handled
    );
    assert!(toolbar.is_visible());
    assert!(toolbar.take_events().is_empty());
    assert!(host.requests.take_invalidation().is_clean());
}

// ------------------------------------------------------------------------ link popup

fn link_popup(anchor: Rect) -> (Host, LinkEditPopup) {
    let mut host = Host::new();
    let mut popup = LinkEditPopup::new(&localizer(), &clipboard()).with_motion(Motion::reduced());
    popup.show(anchor, "Rust", "https://rust-lang.org");
    host.layout(&mut popup, WINDOW);
    let focus = popup.take_focus_request().expect("showing asks for focus");
    host.focus_on(focus);
    (host, popup)
}

#[test]
fn the_link_popup_opens_below_the_link_with_both_fields() {
    let (host, popup) = link_popup(Rect::new(300.0, 100.0, 60.0, 20.0));
    assert_eq!(popup.bounds(), Rect::new(300.0, 126.0, 320.0, 128.0));
    assert_eq!(popup.text_box().text(), "Rust");
    assert_eq!(popup.url_box().text(), "https://rust-lang.org");
    assert_eq!(popup.url_box().selection(), 0..21);
    assert!(host.focus.is_focused(popup.url_box().id()));
    let tree = host.tree(&popup);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.name, "Edit link");
    assert_eq!(
        child_names(&tree, root.id),
        ["Text", "Address", "Remove link", "Apply"]
    );
    assert_eq!(host.focus.tab_order().len(), 4);
    let texts = host.paint(&popup);
    assert!(texts.texts().contains(&"Rust") && texts.texts().contains(&"Apply"));
    // Near the bottom of the window it opens above the link.
    let (_host, low) = link_popup(Rect::new(300.0, 560.0, 60.0, 20.0));
    assert_eq!(low.bounds().y, 426.0);
}

#[test]
fn typing_an_address_and_pressing_enter_applies() {
    let (mut host, mut popup) = link_popup(Rect::new(300.0, 100.0, 60.0, 20.0));
    host.text(&mut popup, "https://example.com");
    assert!(host.key(&mut popup, vk::RETURN).handled);
    assert_eq!(
        popup.take_events(),
        vec![LinkEditEvent::Apply {
            text: "Rust".to_owned(),
            url: "https://example.com".to_owned()
        }]
    );
    assert!(!popup.is_visible());
    // Enter in the text box applies too, with the edited text.
    popup.show(Rect::new(300.0, 100.0, 60.0, 20.0), "Old", "https://a.test");
    host.layout(&mut popup, WINDOW);
    host.focus_on(popup.text_box().id());
    host.key_with(&mut popup, KEY_A, Modifiers::CTRL);
    host.text(&mut popup, "New");
    host.key(&mut popup, vk::RETURN);
    assert_eq!(
        popup.take_events(),
        vec![LinkEditEvent::Apply {
            text: "New".to_owned(),
            url: "https://a.test".to_owned()
        }]
    );
}

#[test]
fn the_buttons_remove_and_apply_and_escape_or_an_outside_press_cancel() {
    let anchor = Rect::new(300.0, 100.0, 60.0, 20.0);
    let (mut host, mut popup) = link_popup(anchor);
    let tree = host.tree(&popup);
    let remove = node_named(&tree, "Remove link").bounds.center();
    host.click(&mut popup, remove);
    assert_eq!(popup.take_events(), vec![LinkEditEvent::Remove]);
    popup.show(anchor, "Rust", "https://rust-lang.org");
    host.layout(&mut popup, WINDOW);
    let tree = host.tree(&popup);
    let apply = node_named(&tree, "Apply").bounds.center();
    host.click(&mut popup, apply);
    assert!(matches!(
        popup.take_events().as_slice(),
        [LinkEditEvent::Apply { text, .. }] if text == "Rust"
    ));
    popup.show(anchor, "Rust", "https://rust-lang.org");
    host.layout(&mut popup, WINDOW);
    host.focus_on(popup.url_box().id());
    assert!(host.key(&mut popup, vk::ESCAPE).handled);
    assert_eq!(popup.take_events(), vec![LinkEditEvent::Cancel]);
    popup.show(anchor, "Rust", "https://rust-lang.org");
    host.layout(&mut popup, WINDOW);
    let inside = Point::new(popup.bounds().x + 1.0, popup.bounds().y + 1.0);
    assert!(host.click(&mut popup, inside).handled);
    assert!(popup.is_visible());
    assert!(!host.click(&mut popup, Point::new(5.0, 5.0)).handled);
    assert_eq!(popup.take_events(), vec![LinkEditEvent::Cancel]);
    // Hidden popups ignore input and draw nothing.
    assert!(!host.key(&mut popup, vk::RETURN).handled);
    host.layout(&mut popup, WINDOW);
    assert!(host.paint(&popup).recorded().is_empty());
    assert!(host.tree(&popup).root().expect("root").state.offscreen);
    // Programmatic apply (the host's default action).
    popup.show(anchor, "T", "u");
    popup.apply();
    assert_eq!(popup.take_events().len(), 1);
}

#[test]
fn a_plain_letter_key_is_left_to_the_editor_and_repaints_nothing() {
    let (mut host, mut popup) = link_popup(Rect::new(300.0, 100.0, 60.0, 20.0));
    host.requests.take_invalidation();
    assert!(!host.key(&mut popup, KEY_A).handled);
    assert!(popup.is_visible());
    assert!(popup.take_events().is_empty());
    assert!(host.requests.take_invalidation().is_clean());
}

#[test]
fn assistive_technology_edits_the_link_fields() {
    let (mut host, mut popup) = link_popup(Rect::new(300.0, 100.0, 60.0, 20.0));
    let tree = host.tree(&popup);
    let text_id = node_named(&tree, "Text").id;
    let apply_id = node_named(&tree, "Apply").id;
    assert!(popup.owns(text_id) && popup.owns(popup.id()));
    assert!(!popup.owns(WidgetId::allocate()));
    let mut cx = host.cx(popup.id());
    assert!(popup.perform_child(&mut cx, text_id, &AccessAction::SetValue("Ferris".into())));
    assert_eq!(popup.text_box().text(), "Ferris");
    assert!(!popup.perform_child(&mut cx, WidgetId::allocate(), &AccessAction::Focus));
    assert!(popup.perform_child(&mut cx, apply_id, &AccessAction::Invoke));
    assert!(matches!(
        popup.take_events().as_slice(),
        [LinkEditEvent::Apply { text, .. }] if text == "Ferris"
    ));
    assert!(!popup.perform_child(&mut cx, apply_id, &AccessAction::Invoke));
}

// ------------------------------------------------------------------------ wikilink completion

fn completion(query: &str) -> (Host, WikilinkCompletion) {
    let mut host = Host::new();
    let mut list = WikilinkCompletion::new(localizer());
    let files: Vec<RelPath> = [
        "notes/ideas.md",
        "notes/idea-box.md",
        "journal/2026-01.md",
        "README.md",
        "img/logo.png",
    ]
    .iter()
    .map(|path| RelPath::parse(path).expect("path"))
    .collect();
    list.set_files(files, vec![2]);
    let doc = Document::new("# Intro\n\n## Setup steps\n\n## Usage\n");
    list.set_outline(&Toc::build(doc.ast(), &doc.text()));
    list.open(Rect::new(100.0, 100.0, 2.0, 20.0), query);
    host.layout(&mut list, WINDOW);
    (host, list)
}

fn labels(list: &WikilinkCompletion) -> Vec<String> {
    list.list()
        .rows()
        .iter()
        .map(|row| row.label.clone())
        .collect()
}

#[test]
fn double_brackets_complete_workspace_documents_recent_first() {
    let (_host, list) = completion("");
    assert!(list.is_visible());
    assert_eq!(
        labels(&list),
        [
            "journal/2026-01.md",
            "notes/ideas.md",
            "notes/idea-box.md",
            "README.md"
        ]
    );
    let readme = list
        .list()
        .rows()
        .iter()
        .find(|row| row.label == "README.md")
        .expect("README");
    assert_eq!(readme.detail, "README");
    assert_eq!(list.bounds(), Rect::new(100.0, 126.0, 360.0, 136.0));
}

#[test]
fn typing_ranks_with_highlights_and_enter_or_tab_accepts() {
    let (mut host, mut list) = completion("");
    list.set_query("idea");
    let mut found = labels(&list);
    found.sort();
    assert_eq!(found, ["notes/idea-box.md", "notes/ideas.md"]);
    for row in list.list().rows() {
        assert_eq!(covered(&row.label, &row.highlights), "idea");
    }
    host.layout(&mut list, WINDOW);
    assert!(host.key(&mut list, vk::DOWN).handled);
    assert!(host.key(&mut list, vk::RETURN).handled);
    let events = list.take_events();
    let [WikilinkEvent::Accept(WikilinkAccept { insert, replace })] = events.as_slice() else {
        panic!("expected one accepted completion, got {events:?}");
    };
    assert!(insert.starts_with("idea"));
    assert_eq!(*replace, 0..4);
    assert!(!list.is_visible());
    // Tab accepts as well; the first row is selected by default.
    list.open(Rect::new(100.0, 100.0, 2.0, 20.0), "read");
    host.layout(&mut list, WINDOW);
    assert!(host.key(&mut list, vk::TAB).handled);
    assert_eq!(
        list.take_events(),
        vec![WikilinkEvent::Accept(WikilinkAccept {
            insert: "README".to_owned(),
            replace: 0..4
        })]
    );
}

#[test]
fn a_hash_completes_the_headings_of_the_current_document() {
    let (mut host, mut list) = completion("#");
    assert_eq!(labels(&list), ["Intro", "Setup steps", "Usage"]);
    let prefixes: Vec<&str> = list
        .list()
        .rows()
        .iter()
        .map(|row| row.prefix.as_str())
        .collect();
    assert_eq!(prefixes, ["H1", "H2", "H2"]);
    list.set_query("#usa");
    assert_eq!(labels(&list), ["Usage"]);
    host.layout(&mut list, WINDOW);
    host.key(&mut list, vk::RETURN);
    assert_eq!(
        list.take_events(),
        vec![WikilinkEvent::Accept(WikilinkAccept {
            insert: "Usage".to_owned(),
            replace: 1..4
        })]
    );
}

#[test]
fn an_alias_or_another_documents_heading_has_nothing_to_complete() {
    let (mut host, mut list) = completion("ideas");
    assert!(list.is_visible());
    list.set_query("ideas|al");
    assert!(!list.is_visible());
    list.set_query("ideas#us");
    assert!(!list.is_visible());
    list.set_query("zzzzqq");
    assert!(!list.is_visible());
    // Nothing visible: keys go to the editor, nothing is painted.
    host.layout(&mut list, WINDOW);
    assert_eq!(list.bounds(), Rect::ZERO);
    assert!(!host.key(&mut list, vk::RETURN).handled);
    assert!(host.paint(&list).recorded().is_empty());
    assert!(host.tree(&list).root().expect("root").state.offscreen);
    assert!(!list.accept());
    list.set_query("ideas");
    assert!(list.is_visible());
}

#[test]
fn the_list_follows_the_keyboard_and_leaves_other_keys_to_the_editor() {
    let (mut host, mut list) = completion("");
    assert!(host.key(&mut list, vk::DOWN).handled);
    assert!(host.key(&mut list, vk::DOWN).handled);
    assert_eq!(list.list().selected(), Some(2));
    assert!(host.key(&mut list, vk::UP).handled);
    assert!(host.key(&mut list, vk::PAGE_DOWN).handled);
    assert!(host.key(&mut list, vk::PAGE_UP).handled);
    assert_eq!(list.list().selected(), Some(0));
    // Letters, Ctrl combinations and text are the editor's.
    assert!(!host.key(&mut list, KEY_A).handled);
    assert!(!host.key_with(&mut list, vk::DOWN, Modifiers::CTRL).handled);
    assert!(!host.text(&mut list, "a").handled);
    // Escape dismisses.
    assert!(host.key(&mut list, vk::ESCAPE).handled);
    assert_eq!(list.take_events(), vec![WikilinkEvent::Dismissed]);
    assert!(!list.is_visible());
    // Programmatic close reports nothing.
    list.open(Rect::new(100.0, 100.0, 2.0, 20.0), "");
    list.close();
    assert!(list.take_events().is_empty());
}

#[test]
fn clicking_a_row_accepts_and_pressing_outside_dismisses() {
    let (mut host, mut list) = completion("");
    let at = list.list().row_rect(3).expect("row").center();
    assert!(host.pointer(&mut list, PointerEventKind::Move, at).handled);
    assert!(
        !host
            .pointer(&mut list, PointerEventKind::Move, Point::new(5.0, 5.0))
            .handled
    );
    assert!(host.click(&mut list, at).handled);
    assert_eq!(
        list.take_events(),
        vec![WikilinkEvent::Accept(WikilinkAccept {
            insert: "README".to_owned(),
            replace: 0..0
        })]
    );
    list.open(Rect::new(100.0, 100.0, 2.0, 20.0), "");
    host.layout(&mut list, WINDOW);
    assert!(!host.click(&mut list, Point::new(5.0, 5.0)).handled);
    assert_eq!(list.take_events(), vec![WikilinkEvent::Dismissed]);
    // Near the bottom the list opens above the caret.
    let (mut host, mut low) = completion("");
    low.open(Rect::new(100.0, 570.0, 2.0, 20.0), "");
    host.layout(&mut low, WINDOW);
    assert_eq!(low.bounds().y, 428.0);
}

#[test]
fn a_press_in_the_panel_padding_chooses_nothing_and_a_still_pointer_repaints_nothing() {
    let (mut host, mut list) = completion("");
    // The padding above the first row is inside the panel but not on a row.
    let padding = Point::new(list.bounds().x + 1.0, list.bounds().y + 1.0);
    assert!(list.bounds().contains(padding));
    assert!(host.click(&mut list, padding).handled);
    assert!(list.is_visible());
    assert!(list.take_events().is_empty());
    // Hovering the same row again, or leaving twice, changes nothing on screen.
    let at = list.list().row_rect(1).expect("row").center();
    host.requests.take_invalidation();
    assert!(host.pointer(&mut list, PointerEventKind::Move, at).handled);
    assert!(!host.requests.take_invalidation().is_clean());
    assert!(host.pointer(&mut list, PointerEventKind::Move, at).handled);
    assert!(host.requests.take_invalidation().is_clean());
    let outside = Point::new(5.0, 5.0);
    assert!(
        !host
            .pointer(&mut list, PointerEventKind::Move, outside)
            .handled
    );
    assert!(!host.requests.take_invalidation().is_clean());
    assert!(
        !host
            .pointer(&mut list, PointerEventKind::Move, outside)
            .handled
    );
    assert!(host.requests.take_invalidation().is_clean());
}

#[test]
fn the_access_tree_lists_the_suggestions_and_rows_can_be_invoked() {
    let (host, mut list) = completion("");
    let tree = host.tree(&list);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.name, "Link suggestions");
    assert_eq!(child_names(&tree, root.id), ["Link suggestions"]);
    let rows: Vec<_> = tree
        .children(tree.first_child(root.id).expect("list").id)
        .collect();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].name.starts_with("journal/2026-01.md"));
    assert!(rows[0].state.selected);
    let (first, second) = (rows[0].id, rows[1].id);
    assert!(list.perform_item(second, &AccessAction::Select));
    assert_eq!(list.list().selected(), Some(1));
    assert!(!list.perform_item(WidgetId::allocate(), &AccessAction::Invoke));
    assert!(!list.perform_item(first, &AccessAction::Toggle));
    assert!(list.perform_item(first, &AccessAction::Invoke));
    assert_eq!(list.take_events().len(), 1);
    assert!(!list.is_visible());
}

// ------------------------------------------------------------------------ measuring, painting, actions

#[test]
fn only_the_block_around_the_caret_decides_the_format_state() {
    let text = "**bold** text\n\nplain words";
    assert!(state_of(text, 4, 4).bold);
    let second = text.find("plain").expect("second paragraph") + 2;
    assert_eq!(state_of(text, second, second), FormatState::default());
    assert_eq!(state_of(text, 4, second), FormatState::default());
}

#[test]
fn the_command_menu_is_a_modal_flyout_that_reports_commands() {
    let mut host = Host::new();
    let mut menu = CommandMenu::new("Insert");
    assert!(!menu.is_open());
    let item = |command: CommandId, label: &str| {
        MenuEntry::Item(MenuItem::new(CommandMenu::command_number(command), label))
    };
    let entries = vec![
        item(CommandId::FormatBold, "Bold"),
        item(CommandId::FormatItalic, "Italic"),
    ];
    let mut cx = host.cx(menu.id());
    menu.open_at(&mut cx, Point::new(50.0, 60.0), entries);
    assert!(menu.is_open());
    assert_eq!(menu.highlighted(), None);
    host.key(&mut menu, vk::DOWN);
    assert_eq!(menu.highlighted(), Some((0, 0)));
    host.key(&mut menu, vk::DOWN);
    assert_eq!(menu.highlighted(), Some((0, 1)));
    // The open menu is painted over the window, takes no layout space and takes no actions
    // of its own.
    assert!(host.paint(&menu).texts().contains(&"Italic"));
    assert_eq!(host.measure(&mut menu, Size::new(300.0, 200.0)), Size::ZERO);
    host.layout(&mut menu, WINDOW);
    let id = menu.id();
    let mut cx = host.cx(id);
    assert!(!Widget::perform(&mut menu, &mut cx, &AccessAction::Invoke));
    host.key(&mut menu, vk::RETURN);
    assert_eq!(
        menu.take_events(),
        [CommandMenuEvent::Command(CommandId::FormatItalic)]
    );
    assert!(!menu.is_open());
}

#[test]
fn the_heading_menu_takes_no_layout_space_and_no_actions_of_its_own() {
    let (mut host, mut menu) = open_heading_menu(Some(1));
    assert_eq!(host.measure(&mut menu, Size::new(300.0, 200.0)), Size::ZERO);
    host.layout(&mut menu, WINDOW);
    let id = menu.id();
    let mut cx = host.cx(id);
    assert!(!Widget::perform(&mut menu, &mut cx, &AccessAction::Invoke));
    assert!(menu.is_open());
}

#[test]
fn the_pill_measures_its_size_while_shown_and_nothing_while_hidden() {
    let (mut host, mut pill) = pill();
    assert_eq!(
        host.measure(&mut pill, Size::new(800.0, 40.0)),
        pill.bounds().size()
    );
    let id = pill.id();
    let mut cx = host.cx(id);
    assert!(!Widget::perform(&mut pill, &mut cx, &AccessAction::Toggle));
    pill.set_visible(false);
    assert_eq!(host.measure(&mut pill, Size::new(800.0, 40.0)), Size::ZERO);
}

#[test]
fn the_toolbar_measures_itself_and_only_paints_what_is_damaged() {
    let (mut host, mut toolbar) = toolbar();
    assert_eq!(
        host.measure(&mut toolbar, Size::new(800.0, 600.0)),
        toolbar.bounds().size()
    );
    let away = Rect::new(0.0, 500.0, 50.0, 50.0);
    assert_eq!(host.paint_damaged(&toolbar, Some(away)).ops().count(), 0);
    let over = toolbar.bounds();
    assert!(host.paint_damaged(&toolbar, Some(over)).ops().count() > 0);
    let id = toolbar.id();
    let mut cx = host.cx(id);
    assert!(!Widget::perform(
        &mut toolbar,
        &mut cx,
        &AccessAction::Invoke
    ));
    toolbar.hide();
    assert_eq!(
        host.measure(&mut toolbar, Size::new(800.0, 600.0)),
        Size::ZERO
    );
}

#[test]
fn the_link_popup_measures_itself_and_takes_no_actions_of_its_own() {
    let (mut host, mut popup) = link_popup(Rect::new(300.0, 100.0, 60.0, 20.0));
    assert_eq!(
        host.measure(&mut popup, Size::new(800.0, 600.0)),
        popup.bounds().size()
    );
    let id = popup.id();
    let mut cx = host.cx(id);
    assert!(!Widget::perform(&mut popup, &mut cx, &AccessAction::Invoke));
    host.key(&mut popup, vk::ESCAPE);
    assert!(!popup.is_visible());
    assert_eq!(
        host.measure(&mut popup, Size::new(800.0, 600.0)),
        Size::ZERO
    );
}

#[test]
fn the_completion_measures_paints_and_takes_no_actions_of_its_own() {
    let (mut host, mut list) = completion("");
    assert_eq!(
        host.measure(&mut list, Size::new(800.0, 600.0)),
        list.bounds().size()
    );
    // Visible, the rows are drawn; outside the damaged area nothing is.
    let canvas = host.paint(&list);
    assert!(canvas.texts().contains(&"README.md"));
    let away = Rect::new(0.0, 500.0, 50.0, 50.0);
    assert_eq!(host.paint_damaged(&list, Some(away)).ops().count(), 0);
    let id = list.id();
    let mut cx = host.cx(id);
    assert!(!Widget::perform(&mut list, &mut cx, &AccessAction::Invoke));
    // Releasing the button over the list is not a choice.
    let at = list.list().row_rect(1).expect("row").center();
    assert!(!host.pointer(&mut list, PointerEventKind::Up, at).handled);
    assert!(list.take_events().is_empty());
    list.close();
    assert_eq!(host.measure(&mut list, Size::new(800.0, 600.0)), Size::ZERO);
}

#[test]
fn sources_fed_after_the_keystroke_fill_the_open_list_and_leave_a_closed_one_alone() {
    let mut list = WikilinkCompletion::new(localizer());
    // `[[#` typed before the host handed over the headings: the list is empty ...
    list.open(Rect::new(100.0, 100.0, 2.0, 20.0), "#");
    assert!(!list.is_visible());
    // ... until they arrive.
    let doc = Document::new("# Intro\n\n## Usage\n");
    list.set_outline(&Toc::build(doc.ast(), &doc.text()));
    assert!(list.is_visible());
    assert_eq!(labels(&list), ["Intro", "Usage"]);
    // The query is kept: the files complete the same text.
    list.set_query("");
    assert!(!list.is_visible());
    let files = vec![RelPath::parse("notes.md").expect("path")];
    list.set_files(files.clone(), Vec::new());
    assert_eq!(labels(&list), ["notes.md"]);
    // A closed list stays closed.
    list.close();
    list.set_files(files, Vec::new());
    assert!(!list.is_visible());
}

#[test]
fn the_wheel_scrolls_a_long_list_of_suggestions() {
    let mut host = Host::new();
    let mut list = WikilinkCompletion::new(localizer());
    let files: Vec<RelPath> = (0..12)
        .map(|n| RelPath::parse(&format!("note-{n:02}.md")).expect("path"))
        .collect();
    list.set_files(files, Vec::new());
    list.open(Rect::new(100.0, 100.0, 2.0, 20.0), "");
    host.layout(&mut list, WINDOW);
    assert_eq!(list.list().first_visible(), 0);
    let inside = list.bounds().center();
    let wheel = |dy: f32| {
        Event::Wheel(WheelEvent {
            delta: Point::new(0.0, dy),
            precise: false,
            position: inside,
            mods: Modifiers::NONE,
        })
    };
    assert!(host.send(&mut list, wheel(-1.0)).handled);
    assert!(list.list().first_visible() > 0);
    // Wheeling back to the top and past it leaves the editor to scroll.
    assert!(host.send(&mut list, wheel(10.0)).handled);
    assert_eq!(list.list().first_visible(), 0);
    assert!(!host.send(&mut list, wheel(10.0)).handled);
}
