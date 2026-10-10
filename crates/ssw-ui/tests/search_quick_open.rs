//! Quick Open (`Ctrl+E`) and the command palette (`Ctrl+Shift+P`): ranking and highlights,
//! recent files first, `#` headings, `>` commands with shortcuts, keyboard and pointer flows,
//! empty states, access tree.

mod search_common;

use search_common::{Host, Shortcuts, child_names, clipboard, covered, localizer, node_named};
use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::toc::Toc;
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, Modifiers, PointerEventKind, WheelEvent, vk};
use ssw_ui::shell::palette::{CommandPalette, ShortcutLabels};
use ssw_ui::shell::quick_open::{QuickOpen, QuickOpenEvent, QuickOpenMode};
use ssw_ui::widget::{Widget, WidgetId};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

const WINDOW: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);

fn files() -> Vec<String> {
    [
        "README.md",
        "notes/ideas.md",
        "notes/idea-box.md",
        "journal/2026-01.md",
        "İstanbul/gezi.md",
        "docs/guide/setup.md",
    ]
    .map(String::from)
    .to_vec()
}

fn outline() -> Toc {
    let doc = Document::new("# Intro\n\n## Setup steps\n\ntext\n\n## Usage\n");
    Toc::build(doc.ast(), &doc.text())
}

fn setup_with(files: Vec<String>, recent: Vec<usize>, mode: QuickOpenMode) -> (Host, QuickOpen) {
    let mut host = Host::new();
    let mut quick =
        QuickOpen::new(localizer(), &Shortcuts, &clipboard()).with_motion(Motion::reduced());
    quick.set_files(files, recent);
    quick.set_outline(&outline());
    quick.open(mode);
    host.layout(&mut quick, WINDOW);
    let focus = quick.take_focus_request().expect("opening asks for focus");
    host.focus_on(focus);
    (host, quick)
}

fn setup(mode: QuickOpenMode) -> (Host, QuickOpen) {
    setup_with(files(), vec![3], mode)
}

fn labels(quick: &QuickOpen) -> Vec<String> {
    quick
        .list()
        .rows()
        .iter()
        .map(|row| row.label.clone())
        .collect()
}

#[test]
fn the_mode_follows_the_first_character() {
    assert_eq!(
        QuickOpenMode::of_text("readme"),
        (QuickOpenMode::Files, "readme")
    );
    assert_eq!(
        QuickOpenMode::of_text("#intro"),
        (QuickOpenMode::Headings, "intro")
    );
    assert_eq!(
        QuickOpenMode::of_text(">bold"),
        (QuickOpenMode::Commands, "bold")
    );
    assert_eq!(QuickOpenMode::of_text(""), (QuickOpenMode::Files, ""));
    assert_eq!(QuickOpenMode::Files.prefix(), "");
    assert_eq!(QuickOpenMode::Headings.prefix(), "#");
    assert_eq!(QuickOpenMode::Commands.prefix(), ">");
}

#[test]
fn an_empty_query_lists_the_recent_files_first() {
    let (_host, quick) = setup(QuickOpenMode::Files);
    assert!(quick.is_visible());
    assert_eq!(quick.mode(), QuickOpenMode::Files);
    assert_eq!(
        labels(&quick),
        [
            "journal/2026-01.md",
            "README.md",
            "notes/ideas.md",
            "notes/idea-box.md",
            "İstanbul/gezi.md",
            "docs/guide/setup.md"
        ]
    );
    assert_eq!(quick.list().selected(), Some(0));
    assert_eq!(quick.panel(), Rect::new(120.0, 48.0, 560.0, 248.0));
}

#[test]
fn typing_ranks_the_files_and_highlights_the_matched_characters() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    host.text(&mut quick, "idea");
    let mut found = labels(&quick);
    found.sort();
    assert_eq!(found, ["notes/idea-box.md", "notes/ideas.md"]);
    for row in quick.list().rows() {
        assert_eq!(covered(&row.label, &row.highlights), "idea");
    }
    // The highlights are painted behind the text.
    let accent = host.theme.tokens.accent.with_alpha(0.3);
    let canvas = host.paint(&quick);
    let marks = canvas
        .ops()
        .filter(|op| {
            matches!(op, ssw_ui::canvas::DrawOp::FillRoundedRect { color, .. } if *color == accent)
        })
        .count();
    assert_eq!(marks, 2);
    assert!(canvas.texts().contains(&"notes/ideas.md"));
}

#[test]
fn turkish_dotted_and_dotless_letters_match_each_other() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    host.text(&mut quick, "istanbul");
    assert_eq!(labels(&quick), ["İstanbul/gezi.md"]);
    assert_eq!(quick.list().rows()[0].highlights[0].start, 0);
    quick.set_input("ISTANBUL");
    assert_eq!(labels(&quick), ["İstanbul/gezi.md"]);
    quick.set_input("gezi");
    assert_eq!(labels(&quick), ["İstanbul/gezi.md"]);
}

#[test]
fn a_hash_searches_the_headings_of_the_document() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    host.text(&mut quick, "#");
    assert_eq!(quick.mode(), QuickOpenMode::Headings);
    assert_eq!(labels(&quick), ["Intro", "Setup steps", "Usage"]);
    let prefixes: Vec<&str> = quick
        .list()
        .rows()
        .iter()
        .map(|row| row.prefix.as_str())
        .collect();
    assert_eq!(prefixes, ["H1", "H2", "H2"]);
    host.text(&mut quick, "set");
    assert_eq!(quick.query(), "set");
    assert_eq!(labels(&quick), ["Setup steps"]);
    assert_eq!(
        covered("Setup steps", &quick.list().rows()[0].highlights),
        "Set"
    );
    assert!(host.key(&mut quick, vk::RETURN).handled);
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::GoToHeading {
            index: 1,
            range: outline().entries()[1].range
        }]
    );
    assert!(!quick.is_visible());
}

#[test]
fn the_palette_lists_every_command_with_its_shortcut() {
    let (mut host, mut quick) = setup(QuickOpenMode::Commands);
    assert_eq!(quick.input().text(), ">");
    assert_eq!(quick.mode(), QuickOpenMode::Commands);
    assert_eq!(quick.list().rows().len(), CommandId::ALL.len());
    let bold = quick
        .list()
        .rows()
        .iter()
        .find(|row| row.label == "Bold")
        .expect("Bold")
        .clone();
    assert_eq!(bold.detail, "Ctrl+B");
    assert_eq!(bold.prefix, "Format");
    let palette = quick
        .list()
        .rows()
        .iter()
        .find(|row| row.label == "Command palette")
        .expect("Command palette");
    assert_eq!(palette.detail, "Ctrl+Shift+P");
    assert_eq!(palette.prefix, "Search");
    // Filtering and running.
    host.text(&mut quick, "bold");
    assert_eq!(quick.list().rows()[0].label, "Bold");
    assert_eq!(covered("Bold", &quick.list().rows()[0].highlights), "Bold");
    host.key(&mut quick, vk::RETURN);
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::RunCommand(CommandId::FormatBold)]
    );
    // Deleting the prefix goes back to files.
    quick.open(QuickOpenMode::Commands);
    host.layout(&mut quick, WINDOW);
    host.key(&mut quick, vk::BACK);
    assert_eq!(quick.mode(), QuickOpenMode::Files);
    assert_eq!(quick.list().rows().len(), 6);
}

#[test]
fn the_palette_model_ranks_labels_and_refreshes_shortcuts() {
    let localizer = localizer();
    let mut palette = CommandPalette::new(&localizer, &Shortcuts);
    assert_eq!(palette.len(), CommandId::ALL.len());
    assert!(!palette.is_empty());
    let all = palette.rank("", usize::MAX);
    assert_eq!(all.len(), CommandId::ALL.len());
    assert_eq!(all[0].command, CommandId::ALL[0]);
    let found = palette.rank("bold", 3);
    assert_eq!(found[0].command, CommandId::FormatBold);
    assert_eq!(found[0].shortcut.as_deref(), Some("Ctrl+B"));
    assert_eq!(found[0].group, "Format");
    assert!(found.len() <= 3);
    struct Rebound;
    impl ShortcutLabels for Rebound {
        fn shortcut_label(&self, command: CommandId) -> Option<String> {
            (command == CommandId::FormatBold).then(|| "Alt+B".to_owned())
        }
    }
    palette.refresh_shortcuts(&Rebound);
    let found = palette.rank("bold", 1);
    assert_eq!(found[0].shortcut.as_deref(), Some("Alt+B"));
    let row = found[0].to_row();
    assert_eq!(row.detail, "Alt+B");
    assert_eq!(row.label, "Bold");
    assert_eq!(row.prefix, "Format");
    // A command without a shortcut shows an empty detail.
    let none = palette.rank("underline", 1);
    assert_eq!(none[0].to_row().detail, "");
}

#[test]
fn up_and_down_move_the_selection_wrapping_and_enter_opens_the_file() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    assert!(host.key(&mut quick, vk::DOWN).handled);
    assert_eq!(quick.list().selected(), Some(1));
    host.key(&mut quick, vk::DOWN);
    host.key(&mut quick, vk::UP);
    assert_eq!(quick.list().selected(), Some(1));
    host.key(&mut quick, vk::UP);
    host.key(&mut quick, vk::UP);
    assert_eq!(quick.list().selected(), Some(5));
    host.key(&mut quick, vk::DOWN);
    assert_eq!(quick.list().selected(), Some(0));
    host.key(&mut quick, vk::DOWN);
    assert!(host.key(&mut quick, vk::RETURN).handled);
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::OpenFile {
            path: "README.md".to_owned()
        }]
    );
    assert!(!quick.is_visible());
}

#[test]
fn long_lists_scroll_with_the_selection_pages_and_the_wheel() {
    let many: Vec<String> = (0..30).map(|index| format!("f{index:02}.md")).collect();
    let (mut host, mut quick) = setup_with(many, Vec::new(), QuickOpenMode::Files);
    assert_eq!(quick.list().visible(), 0..8);
    for _ in 0..10 {
        host.key(&mut quick, vk::DOWN);
    }
    assert_eq!(quick.list().selected(), Some(10));
    assert_eq!(quick.list().visible(), 3..11);
    host.key(&mut quick, vk::PAGE_DOWN);
    assert_eq!(quick.list().selected(), Some(18));
    assert_eq!(quick.list().first_visible(), 11);
    host.key(&mut quick, vk::PAGE_UP);
    assert_eq!(quick.list().selected(), Some(10));
    assert_eq!(quick.list().first_visible(), 10);
    let at = quick.list().row_rect(10).expect("row in view").center();
    let wheel = Event::Wheel(WheelEvent {
        delta: Point::new(0.0, -1.0),
        precise: false,
        position: at,
        mods: Modifiers::NONE,
    });
    assert!(host.send(&mut quick, wheel).handled);
    // One notch scrolls three rows; the selection stays where it was.
    assert_eq!(quick.list().first_visible(), 13);
    assert_eq!(quick.list().selected(), Some(10));
    // The scroll indicator is painted because the list is longer than the view.
    let scrollbar = host.theme.tokens.scrollbar;
    let canvas = host.paint(&quick);
    assert!(canvas.ops().any(|op| {
        matches!(op, ssw_ui::canvas::DrawOp::FillRoundedRect { color, rect, .. }
            if *color == scrollbar && rect.width == 3.0)
    }));
}

#[test]
fn clicking_a_row_opens_it_and_clicking_outside_closes() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    let at = quick.list().row_rect(2).expect("row").center();
    host.pointer(&mut quick, ssw_ui::input::PointerEventKind::Move, at);
    assert!(host.click(&mut quick, at).handled);
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::OpenFile {
            path: "notes/ideas.md".to_owned()
        }]
    );
    quick.open(QuickOpenMode::Files);
    host.layout(&mut quick, WINDOW);
    assert!(!host.click(&mut quick, Point::new(5.0, 5.0)).handled);
    assert!(!quick.is_visible());
    assert_eq!(quick.take_events(), vec![QuickOpenEvent::Closed]);
}

#[test]
fn closing_twice_reports_closed_once() {
    let (_host, mut quick) = setup(QuickOpenMode::Files);
    quick.close();
    assert!(!quick.is_visible());
    assert_eq!(quick.take_events(), vec![QuickOpenEvent::Closed]);
    quick.close();
    assert!(quick.take_events().is_empty());
}

#[test]
fn hovering_the_same_spot_again_keeps_the_highlight_and_the_message_is_not_a_row() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    let hover = host.theme.tokens.hover_bg;
    let list = quick.list().bounds();
    let hovered_rows = |host: &Host, quick: &QuickOpen| {
        host.paint(quick)
            .ops_in(list)
            .into_iter()
            .filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == hover))
            .count()
    };
    let row = quick.list().row_rect(2).expect("row 2").center();
    for _ in 0..2 {
        host.pointer(&mut quick, PointerEventKind::Move, row);
        assert_eq!(hovered_rows(&host, &quick), 1);
    }
    let above = Point::new(row.x, list.y - 10.0);
    for _ in 0..2 {
        host.pointer(&mut quick, PointerEventKind::Move, above);
        assert_eq!(hovered_rows(&host, &quick), 0);
    }
    // With nothing found the message sits where the rows would be: pressing it chooses nothing.
    host.text(&mut quick, "zzzqq");
    host.layout(&mut quick, WINDOW);
    let message = quick.list().bounds().center();
    assert!(quick.panel().contains(message));
    assert!(host.click(&mut quick, message).handled);
    assert!(quick.is_visible());
    assert!(quick.take_events().is_empty());
}

#[test]
fn a_key_the_box_does_not_use_is_left_alone_and_repaints_nothing() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    host.requests.take_invalidation();
    assert!(!host.key(&mut quick, vk::F3).handled);
    assert!(quick.is_visible());
    assert!(quick.take_events().is_empty());
    assert!(host.requests.take_invalidation().is_clean());
}

#[test]
fn escape_closes_and_nothing_matching_shows_a_message() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    host.text(&mut quick, "zzzqq");
    assert!(quick.list().is_empty());
    assert_eq!(quick.empty_text(), "No matching files");
    host.layout(&mut quick, WINDOW);
    assert_eq!(quick.panel(), Rect::new(120.0, 48.0, 560.0, 88.0));
    assert!(host.paint(&quick).texts().contains(&"No matching files"));
    // Enter with nothing selected only consumes the key.
    assert!(host.key(&mut quick, vk::RETURN).handled);
    assert!(quick.is_visible());
    assert!(quick.take_events().is_empty());
    quick.set_input("#qqqq");
    assert_eq!(quick.empty_text(), "No matching headings");
    quick.set_input(">qqqq");
    assert_eq!(quick.empty_text(), "No matching commands");
    assert!(host.key(&mut quick, vk::ESCAPE).handled);
    assert!(!quick.is_visible());
    assert_eq!(quick.take_events(), vec![QuickOpenEvent::Closed]);
    // A hidden overlay ignores input and paints nothing.
    assert!(!host.key(&mut quick, vk::DOWN).handled);
    host.layout(&mut quick, WINDOW);
    assert!(host.paint(&quick).recorded().is_empty());
}

#[test]
fn the_access_tree_describes_the_box_and_the_rows() {
    let (host, mut quick) = setup(QuickOpenMode::Files);
    let tree = host.tree(&quick);
    let root = tree.root().expect("root").clone();
    assert_eq!(root.role, Role::Group);
    assert_eq!(root.name, "Quick Open");
    assert_eq!(child_names(&tree, root.id), ["Quick Open", "Results"]);
    let list = node_named(&tree, "Results").clone();
    assert_eq!(list.role, Role::List);
    let rows: Vec<_> = tree.children(list.id).collect();
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0].name, "journal/2026-01.md");
    assert!(rows[0].state.selected && !rows[1].state.selected);
    assert_eq!(rows[0].role, Role::ListItem);
    // Command rows read group, label and shortcut.
    quick.set_input(">bold");
    let tree = host.tree(&quick);
    assert_eq!(
        tree.children(node_named(&tree, "Results").id)
            .next()
            .expect("row")
            .name,
        "Format Bold Ctrl+B"
    );
    // An empty result list is a text node.
    quick.set_input("zzzzqq");
    let tree = host.tree(&quick);
    let message = tree
        .children(root.id)
        .find(|node| node.role == Role::Text)
        .expect("message");
    assert_eq!(message.name, "No matching files");
    // Hidden: offscreen.
    quick.close();
    let tree = host.tree(&quick);
    assert!(tree.root().expect("root").state.offscreen);
}

#[test]
fn assistive_technology_can_type_select_and_invoke() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    let tree = host.tree(&quick);
    let input_id = node_named(&tree, "Quick Open").clone().id;
    let list_id = node_named(&tree, "Results").id;
    let row_ids: Vec<WidgetId> = tree.children(list_id).map(|node| node.id).collect();
    assert!(quick.owns(input_id) && quick.owns(list_id) && quick.owns(quick.id()));
    let mut cx = host.cx(quick.id());
    assert!(quick.perform_child(&mut cx, input_id, &AccessAction::SetValue("idea".into())));
    assert_eq!(quick.list().rows().len(), 2);
    assert!(!quick.perform_child(&mut cx, WidgetId::allocate(), &AccessAction::Invoke));
    // Rows were replaced by the search: the old ids are gone.
    assert!(!quick.perform_child(&mut cx, row_ids[0], &AccessAction::Invoke));
    let tree = host.tree(&quick);
    let new_rows: Vec<_> = tree
        .children(node_named(&tree, "Results").id)
        .map(|node| node.id)
        .collect();
    let mut cx = host.cx(quick.id());
    assert!(quick.perform_child(&mut cx, new_rows[1], &AccessAction::Select));
    assert_eq!(quick.list().selected(), Some(1));
    assert!(quick.perform_child(&mut cx, new_rows[1], &AccessAction::Invoke));
    assert_eq!(quick.take_events().len(), 1);
    // Once hidden nothing is accepted.
    assert!(!quick.perform_child(&mut cx, input_id, &AccessAction::Focus));
}

#[test]
fn new_shortcuts_appear_without_reopening() {
    struct Nothing;
    impl ShortcutLabels for Nothing {
        fn shortcut_label(&self, _: CommandId) -> Option<String> {
            None
        }
    }
    let (_host, mut quick) = setup(QuickOpenMode::Commands);
    assert!(quick.list().rows().iter().any(|row| !row.detail.is_empty()));
    quick.refresh_shortcuts(&Nothing);
    assert!(quick.list().rows().iter().all(|row| row.detail.is_empty()));
}

#[test]
fn the_overlay_measures_its_panel_and_nothing_while_hidden() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    let panel = host.measure(&mut quick, Size::new(800.0, 600.0));
    assert_eq!(panel.width, 560.0);
    assert!(panel.height > 0.0);
    // A narrow window leaves the padding on both sides; an unbounded one gets the widest panel.
    assert_eq!(
        host.measure(&mut quick, Size::new(300.0, 600.0)),
        Size::new(284.0, panel.height)
    );
    assert_eq!(host.measure(&mut quick, Size::UNBOUNDED), panel);
    quick.close();
    assert_eq!(
        host.measure(&mut quick, Size::new(800.0, 600.0)),
        Size::ZERO
    );
}

#[test]
fn the_pointer_highlights_the_hovered_row_until_it_moves_away_or_leaves() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    let hover = host.theme.tokens.hover_bg;
    let list = quick.list().bounds();
    let hovered_rows = |host: &Host, quick: &QuickOpen| {
        host.paint(quick)
            .ops_in(list)
            .into_iter()
            .filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == hover))
            .count()
    };
    assert_eq!(hovered_rows(&host, &quick), 0);
    let row = quick.list().row_rect(2).expect("row 2").center();
    // The overlay is modal: a pointer over its panel is always taken.
    assert!(
        host.pointer(&mut quick, PointerEventKind::Move, row)
            .handled
    );
    assert_eq!(hovered_rows(&host, &quick), 1);
    // Moving above the list clears the highlight; so does leaving the overlay.
    let above = Point::new(row.x, quick.list().bounds().y - 10.0);
    host.pointer(&mut quick, PointerEventKind::Move, above);
    assert_eq!(hovered_rows(&host, &quick), 0);
    host.pointer(&mut quick, PointerEventKind::Move, row);
    assert_eq!(hovered_rows(&host, &quick), 1);
    host.pointer(&mut quick, PointerEventKind::Leave, row);
    assert_eq!(hovered_rows(&host, &quick), 0);
    host.pointer(&mut quick, PointerEventKind::Leave, row);
    assert_eq!(hovered_rows(&host, &quick), 0);
    // Releasing the button over a row chooses nothing.
    assert!(host.pointer(&mut quick, PointerEventKind::Up, row).handled);
    assert!(quick.take_events().is_empty());
}

#[test]
fn clicking_the_query_box_takes_the_focus_back() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    host.focus.blur();
    let at = quick.input().bounds().center();
    host.click(&mut quick, at);
    assert_eq!(host.focus.focused(), Some(quick.input().id()));
}

#[test]
fn the_overlay_itself_takes_no_accessibility_actions() {
    let (mut host, mut quick) = setup(QuickOpenMode::Files);
    let id = quick.id();
    let mut cx = host.cx(id);
    for action in [AccessAction::Invoke, AccessAction::Focus] {
        assert!(!Widget::perform(&mut quick, &mut cx, &action));
    }
}
