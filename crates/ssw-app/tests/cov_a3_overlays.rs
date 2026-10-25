//! The overlays over a real editor in a laid-out window (spec §2.9, §3.2, §7): input is
//! delivered the way the shell does (layout, focus requests granted, events routed topmost
//! first, `pump` applying the decisions to the editor) and the results are read back from the
//! document, the editor selection, the widgets and the actions the controller would get.

use std::cell::RefCell;
use std::rc::Rc;

use ssw_app::i18n::localizer_for;
use ssw_app::ui::overlays::{NoShortcuts, OverlayAction, OverlayEnv, OverlayRects, Overlays};
use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::edit::{Edit, Selection, Transaction};
use ssw_core::i18n::Localizer;
use ssw_core::keymap::Keymap;
use ssw_core::links::RelPath;
use ssw_core::settings::{ExportFormat, ExportSettings};
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;
use ssw_ui::access::{AccessTree, Role};
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::editor::{EditorEvent, EditorView};
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::input::{
    Event, FocusEvent, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind,
    TextInput, vk,
};
use ssw_ui::services::{Clock, FakeClock, MemoryClipboard};
use ssw_ui::shell::folder_search::{
    FolderSearchFile, FolderSearchHit, FolderSearchLine, FolderSearchRequest, FolderSearchSummary,
};
use ssw_ui::shell::quick_open::QuickOpenMode;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{
    AccessCx, EventResult, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests, Widget,
    WidgetId,
};
use ssw_ui::widgets::anim::Motion;
use ssw_ui::widgets::{Control, SharedClipboard};

const WINDOW: Rect = Rect::new(0.0, 0.0, 900.0, 600.0);
const SIDEBAR: Rect = Rect::new(0.0, 0.0, 260.0, 560.0);
const EDITOR: Rect = Rect::new(260.0, 0.0, 640.0, 560.0);

struct Rig {
    overlays: Overlays,
    doc: Rc<RefCell<Document>>,
    editor: EditorView,
    localizer: Rc<Localizer>,
    theme: Theme,
    engine: MonoTextEngine,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
}

fn key(code: u16) -> Event {
    Event::Key(KeyEvent::press(code, Modifiers::NONE))
}

fn typed(text: &str) -> Event {
    Event::Text(TextInput(text.to_owned()))
}

fn mouse(kind: PointerEventKind, at: Point) -> Event {
    let button = matches!(kind, PointerEventKind::Down | PointerEventKind::Up)
        .then_some(PointerButton::Left);
    Event::Pointer(PointerEvent {
        click_count: 1,
        ..PointerEvent::mouse(kind, button, at)
    })
}

fn rel(path: &str) -> RelPath {
    RelPath::parse(path).unwrap()
}

impl Rig {
    fn new(text: &str) -> Self {
        let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
        let localizer = Rc::new(localizer_for("en"));
        let mut overlays = Overlays::new(&localizer, &clipboard, &NoShortcuts);
        let doc = Rc::new(RefCell::new(Document::new(text)));
        overlays.set_document(&doc);
        let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
        let editor = EditorView::new(Rc::clone(&doc), clock);
        let mut rig = Self {
            overlays,
            doc,
            editor,
            localizer,
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
        };
        rig.layout();
        rig
    }

    /// A rig whose `[[` completion knows two files and whose caret is at `caret`.
    fn wiki(text: &str, caret: usize) -> Self {
        let mut rig = Self::new(text);
        rig.overlays.set_completion_sources(
            vec![rel("alpha.md"), rel("beta.md")],
            Vec::new(),
            &Toc::default(),
        );
        rig.place_caret(caret);
        rig
    }

    /// Layout pass, then the focus requests of the panels that just opened are granted.
    fn layout(&mut self) {
        self.focus.begin_tab_order();
        {
            let id = self.editor.id();
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.requests,
            );
            self.editor.measure(&mut cx, EDITOR.size());
            self.editor.arrange(&mut cx, EDITOR);
            self.overlays.arrange(
                &mut cx,
                &OverlayRects {
                    window: WINDOW,
                    editor: EDITOR,
                    sidebar: Some(SIDEBAR),
                },
            );
        }
        self.focus.commit_tab_order();
        for id in self.overlays.take_focus_requests() {
            self.focus.focus(id);
        }
    }

    fn deliver(&mut self, event: Event) -> EventResult {
        let mut env = OverlayEnv {
            theme: &self.theme,
            scale: 1.0,
            text: &self.engine,
            focus: &mut self.focus,
            capture: &mut self.capture,
            requests: &mut self.requests,
        };
        self.overlays.event(&event, &mut env)
    }

    fn press(&mut self, code: u16) -> EventResult {
        self.deliver(key(code))
    }

    fn click(&mut self, at: Point) {
        self.deliver(mouse(PointerEventKind::Down, at));
        self.deliver(mouse(PointerEventKind::Up, at));
    }

    fn flush(&mut self) {
        let mut env = OverlayEnv {
            theme: &self.theme,
            scale: 1.0,
            text: &self.engine,
            focus: &mut self.focus,
            capture: &mut self.capture,
            requests: &mut self.requests,
        };
        self.overlays.flush(&mut env);
    }

    fn pump(&mut self) -> Vec<OverlayAction> {
        self.overlays.pump(Some(&mut self.editor))
    }

    fn editor_event(&mut self, event: EditorEvent) -> Option<EditorEvent> {
        self.overlays.editor_event(event, &self.editor)
    }

    fn place_caret(&mut self, offset: usize) {
        self.editor.set_selection(Selection::caret(offset));
    }

    fn text(&self) -> String {
        self.doc.borrow().text()
    }

    fn toc(&self) -> Toc {
        let doc = self.doc.borrow();
        Toc::build(doc.ast(), &doc.text())
    }

    fn access(&self) -> (AccessTree, Vec<WidgetId>) {
        let root = self.editor.id();
        let mut tree = AccessTree::new(root);
        let cx = AccessCx::new(root, &self.theme, 1.0, &self.engine, &self.focus);
        let ids = self.overlays.access(&cx, &mut tree);
        (tree, ids)
    }

    fn button_center(&self, name: &str) -> Point {
        let (tree, _) = self.access();
        tree.nodes()
            .iter()
            .find(|node| node.role == Role::Button && node.name == name)
            .unwrap_or_else(|| panic!("no button named {name:?}"))
            .bounds
            .center()
    }

    fn paint_overlays(&self, sidebar: Option<Rect>) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            self.editor.id(),
            &self.theme,
            1.0,
            &self.engine,
            &self.focus,
            &self.capture,
            None,
        );
        self.overlays.paint(&cx, &mut canvas, sidebar);
        assert!(canvas.is_balanced());
        canvas
    }

    /// The search highlight colors the editor paints, in paint order.
    fn highlight_fills(&self) -> Vec<Color> {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            self.editor.id(),
            &self.theme,
            1.0,
            &self.engine,
            &self.focus,
            &self.capture,
            None,
        );
        self.editor.paint(&cx, &mut canvas);
        let palette = &self.theme.tokens.highlight;
        let (current, other) = (palette.orange.background, palette.yellow.background);
        canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRoundedRect { color, .. } if *color == current || *color == other => {
                    Some(*color)
                }
                _ => None,
            })
            .collect()
    }

    fn open_heading_menu(&mut self, level: u8) {
        let anchor = Rect::new(280.0, 40.0, 36.0, 24.0);
        let event = EditorEvent::HeadingMenu {
            range: TextRange::new(0, 1),
            level,
            anchor,
        };
        assert_eq!(self.editor_event(event), None);
        self.flush();
    }

    fn show_toolbar(&mut self) {
        let anchor = Rect::new(400.0, 200.0, 80.0, 20.0);
        assert_eq!(
            self.editor_event(EditorEvent::SelectionToolbar { anchor }),
            None
        );
        self.layout();
    }
}

fn wiki_labels(rig: &Rig) -> Vec<String> {
    let mut labels: Vec<String> = rig
        .overlays
        .wikilink()
        .list()
        .rows()
        .iter()
        .map(|row| row.label.clone())
        .collect();
    labels.sort();
    labels
}

const LINK_DOC: &str = "go [Rust](https://rust-lang.org) now\nlast line\n";

// ----- debug output, keymap, opening -----------------------------------------------------------

#[test]
fn debug_output_names_what_is_open_and_the_env_its_scale() {
    let mut rig = Rig::new("alpha\n");
    assert_eq!(
        format!("{:?}", rig.overlays),
        "Overlays { find: false, goto: false, quick_open: false, folder_search: false, .. }"
    );
    rig.overlays.open_find(false);
    rig.overlays.open_goto();
    rig.overlays.open_quick_open(
        QuickOpenMode::Files,
        Vec::new(),
        Vec::new(),
        &Toc::default(),
    );
    rig.overlays.open_folder_search(None);
    assert_eq!(
        format!("{:?}", rig.overlays),
        "Overlays { find: true, goto: true, quick_open: true, folder_search: true, .. }"
    );
    let env = OverlayEnv {
        theme: &rig.theme,
        scale: 2.0,
        text: &rig.engine,
        focus: &mut rig.focus,
        capture: &mut rig.capture,
        requests: &mut rig.requests,
    };
    assert_eq!(format!("{env:?}"), "OverlayEnv { scale: 2.0, .. }");
}

#[test]
fn a_new_keymap_leaves_a_shown_toolbar_and_an_open_heading_menu_alone() {
    let mut rig = Rig::new("# Title\n");
    rig.open_heading_menu(1);
    rig.show_toolbar();
    // The toolbar and the menu are open while the keymap changes: they stay.
    assert!(rig.overlays.toolbar_visible() && rig.overlays.heading_menu().is_open());
    rig.overlays.set_keymap(Keymap::default());
    assert!(rig.overlays.toolbar().is_visible());
    assert!(rig.overlays.heading_menu().is_open());
}

#[test]
fn f3_opens_a_closed_find_bar_without_moving_the_selection() {
    let mut rig = Rig::new("one two one\n");
    rig.overlays.find_next();
    assert!(rig.overlays.find_visible());
    assert_eq!(rig.editor.selection(), Selection::caret(0));
    rig.overlays.find_mut().close();
    assert!(!rig.overlays.find_visible());
    rig.overlays.find_previous();
    assert!(rig.overlays.find_visible());
    assert_eq!(rig.editor.selection(), Selection::caret(0));
}

#[test]
fn a_folder_search_seed_fills_the_query_only_when_it_is_one_line() {
    let mut rig = Rig::new("alpha\n");
    rig.overlays.open_folder_search(Some("needle"));
    assert!(rig.overlays.folder_search_visible());
    assert_eq!(rig.overlays.folder_search().input().text(), "needle");
    for seed in [None, Some(""), Some("two\nlines"), Some("a\rb")] {
        rig.overlays.open_folder_search(seed);
        assert_eq!(rig.overlays.folder_search().input().text(), "needle");
    }
}

#[test]
fn the_export_dialog_is_modal_and_escape_closes_it() {
    let mut rig = Rig::new("body\n");
    assert!(rig.overlays.export_dialog().is_none());
    let defaults = ExportSettings {
        default_format: ExportFormat::Png,
        ..ExportSettings::default()
    };
    rig.overlays.open_export(&defaults, Motion::default());
    assert!(rig.overlays.export_visible());
    assert_eq!(
        rig.overlays.export_dialog().unwrap().format(),
        ExportFormat::Png
    );
    rig.layout();
    // Typing is taken by the dialog; a focus change is not for it.
    assert!(rig.deliver(typed("z")).handled);
    assert!(!rig.deliver(Event::Focus(FocusEvent::Gained)).handled);
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.export_visible());
    assert!(rig.overlays.take_refocus());
    assert!(rig.pump().is_empty());
}

// ----- folder search ---------------------------------------------------------------------------

#[test]
fn folder_results_reach_the_panel_and_its_choices_become_actions() {
    let mut rig = Rig::new("text\n");
    rig.overlays.open_folder_search(None);
    let files = vec![FolderSearchFile {
        path: "notes/a.md".to_owned(),
        matches: vec![FolderSearchLine {
            line: 3,
            column: 5,
            ranges: vec![5..11],
            preview: "some needle here".to_owned(),
        }],
    }];
    rig.overlays
        .set_folder_results(files.clone(), FolderSearchSummary { truncated: false });
    assert_eq!(rig.overlays.folder_search().files(), files.as_slice());
    // A file header and its match.
    assert_eq!(rig.overlays.folder_search().list().rows().len(), 2);
    assert!(rig.overlays.folder_search_mut().activate_row(1));
    assert_eq!(
        rig.pump(),
        [OverlayAction::OpenMatch(FolderSearchHit {
            path: "notes/a.md".to_owned(),
            line: 3,
            range: 5..11,
        })]
    );
    rig.overlays.folder_search_mut().clear();
    assert_eq!(rig.pump(), [OverlayAction::FolderSearchCleared]);
    assert!(rig.overlays.folder_search().files().is_empty());
}

#[test]
fn typing_a_query_and_enter_asks_for_a_search_and_escape_closes_the_focused_panel() {
    let mut rig = Rig::new("text\n");
    rig.overlays.open_folder_search(None);
    rig.layout();
    assert!(rig.deliver(typed("needle")).handled);
    assert_eq!(rig.overlays.folder_search().input().text(), "needle");
    assert!(rig.press(vk::RETURN).handled);
    assert_eq!(
        rig.pump(),
        [OverlayAction::FolderSearch(FolderSearchRequest {
            pattern: "needle".to_owned(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
        })]
    );
    // With the focus in the editor Esc is not for the panel.
    let editor_id = rig.editor.id();
    rig.focus.focus(editor_id);
    assert_eq!(rig.focus.focused(), Some(editor_id));
    assert!(!rig.press(vk::ESCAPE).handled);
    assert!(rig.overlays.folder_search_visible());
    // With the focus in its query box it closes the panel.
    let input_id = rig.overlays.folder_search().input().id();
    rig.focus.focus(input_id);
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.folder_search_visible());
    assert!(rig.overlays.take_refocus());
}

// ----- layout, paint, accessibility ------------------------------------------------------------

#[test]
fn the_folder_panel_fills_the_sidebar_and_is_described_only_while_shown() {
    let mut rig = Rig::new("text\n");
    assert_eq!(rig.overlays.folder_search().bounds(), Rect::ZERO);
    let (tree, ids) = rig.access();
    assert_eq!(ids.len(), 7);
    let find_id = rig.overlays.find().id();
    assert!(tree.node(find_id).unwrap().state.offscreen);
    let fill = |canvas: &RecordingCanvas, sidebar: Rect, color: Color| {
        canvas.ops().any(
            |op| matches!(op, DrawOp::FillRect { rect, color: c } if *rect == sidebar && *c == color),
        )
    };
    let sidebar_bg = rig.theme.tokens.sidebar_bg;
    assert!(!fill(
        &rig.paint_overlays(Some(SIDEBAR)),
        SIDEBAR,
        sidebar_bg
    ));

    rig.overlays.open_folder_search(None);
    rig.layout();
    assert_eq!(rig.overlays.folder_search().bounds(), SIDEBAR);
    assert!(fill(
        &rig.paint_overlays(Some(SIDEBAR)),
        SIDEBAR,
        sidebar_bg
    ));
    assert!(!fill(&rig.paint_overlays(None), SIDEBAR, sidebar_bg));
    let (tree, ids) = rig.access();
    assert_eq!(ids.len(), 8);
    assert!(ids.contains(&rig.overlays.folder_search().id()));
    assert!(
        !tree
            .node(rig.overlays.folder_search().id())
            .unwrap()
            .state
            .offscreen
    );

    rig.overlays
        .open_export(&ExportSettings::default(), Motion::default());
    rig.layout();
    let (tree, ids) = rig.access();
    assert_eq!(ids.len(), 9);
    assert!(!tree.node(*ids.last().unwrap()).unwrap().state.offscreen);
}

#[test]
fn any_open_overlay_captures_the_keys() {
    let mut rig = Rig::wiki("see [[al", 0);
    assert!(!rig.overlays.captures_keys());

    rig.overlays.open_find(false);
    assert!(rig.overlays.captures_keys());
    rig.overlays.find_mut().close();
    assert!(!rig.overlays.captures_keys());

    rig.overlays.open_goto();
    assert!(rig.overlays.captures_keys());
    rig.overlays.goto_mut().close();
    assert!(!rig.overlays.captures_keys());

    rig.overlays.open_quick_open(
        QuickOpenMode::Files,
        Vec::new(),
        Vec::new(),
        &Toc::default(),
    );
    assert!(rig.overlays.captures_keys());
    rig.overlays.quick_open_mut().close();
    assert!(!rig.overlays.captures_keys());

    rig.overlays.open_folder_search(None);
    assert!(rig.overlays.captures_keys());
    rig.overlays.close_folder_search();
    assert!(!rig.overlays.captures_keys());

    rig.overlays.open_link(&rig.editor);
    assert!(rig.overlays.link_visible() && rig.overlays.captures_keys());
    rig.overlays.link_mut().apply();
    assert!(!rig.overlays.link_visible() && !rig.overlays.captures_keys());

    rig.show_toolbar();
    assert!(rig.overlays.captures_keys());
    rig.overlays.toolbar_mut().hide();
    assert!(!rig.overlays.captures_keys());

    rig.open_heading_menu(1);
    assert!(rig.overlays.captures_keys());
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.captures_keys());

    rig.overlays
        .open_export(&ExportSettings::default(), Motion::default());
    assert!(rig.overlays.captures_keys());
    rig.layout();
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.captures_keys());

    rig.place_caret(8);
    rig.editor_event(EditorEvent::Edited);
    assert!(rig.overlays.wikilink_visible() && rig.overlays.captures_keys());
    rig.overlays.wikilink_mut().close();
    assert!(!rig.overlays.captures_keys());
}

// ----- event routing ---------------------------------------------------------------------------

#[test]
fn an_event_no_overlay_wants_stays_unhandled_with_every_overlay_open() {
    let mut rig = Rig::wiki("see [[al", 8);
    rig.editor_event(EditorEvent::Edited);
    rig.overlays.open_find(false);
    rig.overlays.open_goto();
    rig.overlays.open_quick_open(
        QuickOpenMode::Files,
        Vec::new(),
        Vec::new(),
        &Toc::default(),
    );
    rig.overlays.open_folder_search(None);
    rig.overlays.open_link(&rig.editor);
    rig.open_heading_menu(1);
    rig.show_toolbar();
    rig.overlays
        .open_export(&ExportSettings::default(), Motion::default());
    rig.layout();
    assert!(rig.overlays.wikilink_visible());
    assert!(rig.overlays.toolbar_visible());
    assert!(rig.overlays.heading_menu().is_open());
    assert!(rig.overlays.export_visible());
    assert!(!rig.deliver(Event::Focus(FocusEvent::Gained)).handled);
}

#[test]
fn escape_closes_the_find_bar_even_when_the_editor_has_the_focus() {
    let mut rig = Rig::new("one two one\n");
    rig.overlays.open_find(false);
    // Not laid out: no control of the bar has the focus.
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.find_visible());
    assert!(rig.pump().is_empty());
    assert!(rig.overlays.take_refocus());
    // Another key the bar does not use reaches the editor.
    rig.overlays.open_find(false);
    assert!(!rig.press(vk::F3 + 1).handled);
    assert!(rig.overlays.find_visible());
}

#[test]
fn the_find_bar_takes_typing_and_f3_before_the_editor_does() {
    let mut rig = Rig::new("one two one\n");
    rig.overlays.open_find(false);
    // The layout grants the focus to the bar's search field.
    rig.layout();
    assert!(rig.deliver(typed("one")).handled);
    assert_eq!(rig.overlays.find().find_box().text(), "one");
    assert_eq!(
        rig.overlays.find().matches(),
        [TextRange::new(0, 3), TextRange::new(8, 11)]
    );
    assert!(rig.pump().is_empty());
    assert_eq!(rig.editor.selection(), Selection::new(0, 3));
    // F3 steps to the next match; the bar answers, so the editor never sees the key.
    assert!(rig.press(vk::F3).handled);
    assert!(rig.pump().is_empty());
    assert_eq!(rig.editor.selection(), Selection::new(8, 11));
}

#[test]
fn typing_hides_the_toolbar_and_the_editor_still_gets_the_text() {
    let mut rig = Rig::new("some words here\n");
    rig.show_toolbar();
    let result = rig.deliver(typed("x"));
    assert!(!result.handled);
    assert!(!rig.overlays.toolbar_visible());
    assert!(rig.pump().is_empty());
}

#[test]
fn a_toolbar_button_becomes_its_command_and_escape_hides_the_toolbar() {
    let mut rig = Rig::new("some words here\n");
    rig.show_toolbar();
    let bold = rig.localizer.text(CommandId::FormatBold.label_id());
    let at = rig.button_center(&bold);
    rig.click(at);
    assert_eq!(
        rig.pump(),
        [OverlayAction::RunCommand(CommandId::FormatBold)]
    );
    assert!(rig.overlays.toolbar_visible());
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.toolbar_visible());
    assert!(rig.pump().is_empty());
}

#[test]
fn the_heading_menu_runs_the_chosen_level_and_escape_returns_to_the_editor() {
    let mut rig = Rig::new("## Title\n");
    // Nothing is pending: flushing opens nothing.
    rig.flush();
    assert!(!rig.overlays.heading_menu().is_open());
    rig.open_heading_menu(2);
    assert!(rig.overlays.heading_menu().is_open());
    for _ in 0..3 {
        assert!(rig.press(vk::DOWN).handled);
    }
    assert!(rig.press(vk::RETURN).handled);
    assert!(!rig.overlays.heading_menu().is_open());
    assert_eq!(
        rig.pump(),
        [OverlayAction::RunCommand(CommandId::FormatHeading2)]
    );
    assert!(rig.overlays.take_refocus());

    rig.open_heading_menu(2);
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(rig.pump().is_empty());
    assert!(rig.overlays.take_refocus());
}

#[test]
fn editor_events_that_open_nothing_are_handed_back() {
    let mut rig = Rig::new("alpha\n");
    let zoom = EditorEvent::ZoomChanged(150);
    assert_eq!(rig.editor_event(zoom.clone()), Some(zoom));
    let moved = EditorEvent::SelectionChanged(Selection::caret(0));
    assert_eq!(rig.editor_event(moved.clone()), Some(moved));
    assert_eq!(
        rig.editor_event(EditorEvent::Edited),
        Some(EditorEvent::Edited)
    );
    assert!(!rig.overlays.wikilink_visible());
}

// ----- the link popup --------------------------------------------------------------------------

#[test]
fn the_link_popup_shows_the_link_at_the_caret_or_the_selection() {
    let mut rig = Rig::new(LINK_DOC);
    rig.place_caret(5);
    rig.overlays.open_link(&rig.editor);
    assert!(rig.overlays.link_visible());
    assert_eq!(rig.overlays.link().text_box().text(), "Rust");
    assert_eq!(
        rig.overlays.link().url_box().text(),
        "https://rust-lang.org"
    );

    rig.editor.set_selection(Selection::new(0, 2));
    rig.overlays.open_link(&rig.editor);
    assert_eq!(rig.overlays.link().text_box().text(), "go");
    assert_eq!(rig.overlays.link().url_box().text(), "");

    // A selection over a line break gives no link text.
    rig.editor.set_selection(Selection::new(33, 43));
    rig.overlays.open_link(&rig.editor);
    assert_eq!(rig.overlays.link().text_box().text(), "");
    assert_eq!(rig.overlays.link().url_box().text(), "");
}

#[test]
fn the_link_popup_edits_removes_and_cancels() {
    let mut rig = Rig::new(LINK_DOC);
    // Typing an address (with stray blanks) and Enter rewrites the link.
    rig.place_caret(5);
    rig.overlays.open_link(&rig.editor);
    rig.layout();
    assert!(rig.deliver(typed(" https://example.org ")).handled);
    assert!(rig.press(vk::RETURN).handled);
    assert!(!rig.overlays.link_visible());
    assert!(rig.pump().is_empty());
    assert_eq!(
        rig.text(),
        "go [Rust](https://example.org) now\nlast line\n"
    );
    assert!(rig.overlays.take_refocus());

    // *Remove link* keeps the text.
    rig.place_caret(5);
    rig.overlays.open_link(&rig.editor);
    rig.layout();
    let remove = rig.localizer.text("search-link-remove");
    let at = rig.button_center(&remove);
    rig.click(at);
    assert!(!rig.overlays.link_visible());
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "go Rust now\nlast line\n");

    // An address emptied on plain text changes nothing.
    rig.editor.set_selection(Selection::new(3, 7));
    rig.overlays.open_link(&rig.editor);
    rig.layout();
    assert_eq!(rig.overlays.link().text_box().text(), "Rust");
    assert!(rig.press(vk::RETURN).handled);
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "go Rust now\nlast line\n");

    // Esc cancels.
    rig.overlays.open_link(&rig.editor);
    rig.layout();
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.link_visible());
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "go Rust now\nlast line\n");
    assert!(rig.overlays.take_refocus());
}

// ----- `[[` completion -------------------------------------------------------------------------

#[test]
fn enter_completes_an_unclosed_wikilink_and_closes_the_brackets() {
    let mut rig = Rig::wiki("see [[al", 8);
    assert_eq!(
        rig.editor_event(EditorEvent::Edited),
        Some(EditorEvent::Edited)
    );
    assert!(rig.overlays.wikilink_visible());
    assert_eq!(wiki_labels(&rig), ["alpha.md"]);
    assert!(rig.press(vk::RETURN).handled);
    assert!(!rig.overlays.wikilink_visible());
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "see [[alpha]]");
    assert_eq!(rig.editor.selection(), Selection::caret(13));
}

#[test]
fn completing_before_existing_brackets_moves_the_caret_past_them() {
    let mut rig = Rig::wiki("see [[al]] end", 8);
    let moved = EditorEvent::SelectionChanged(Selection::caret(8));
    assert_eq!(rig.editor_event(moved.clone()), Some(moved));
    assert!(rig.press(vk::RETURN).handled);
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "see [[alpha]] end");
    assert_eq!(rig.editor.selection(), Selection::caret(13));
}

#[test]
fn the_completion_follows_the_typing_and_closes_when_the_caret_leaves_the_link() {
    let mut rig = Rig::wiki("see [[a", 7);
    rig.editor_event(EditorEvent::Edited);
    assert_eq!(wiki_labels(&rig), ["alpha.md", "beta.md"]);
    // The user types `l`: the list narrows.
    let typed_l = Transaction::single(
        Edit::insert(7, "l"),
        Selection::caret(7),
        Selection::caret(8),
    );
    rig.doc.borrow_mut().apply(typed_l).unwrap();
    rig.editor_event(EditorEvent::Edited);
    assert_eq!(wiki_labels(&rig), ["alpha.md"]);
    // A selection is not a link being typed.
    rig.editor.set_selection(Selection::new(6, 8));
    rig.editor_event(EditorEvent::SelectionChanged(Selection::new(6, 8)));
    assert!(!rig.overlays.wikilink_visible());
    // Neither is a caret away from the `[[`.
    rig.place_caret(8);
    rig.editor_event(EditorEvent::Edited);
    assert!(rig.overlays.wikilink_visible());
    rig.place_caret(2);
    rig.editor_event(EditorEvent::Edited);
    assert!(!rig.overlays.wikilink_visible());
}

#[test]
fn a_closed_or_nested_wikilink_is_not_completed() {
    for (text, caret) in [("see [[al]]", 10), ("see [[a[l", 9)] {
        let mut rig = Rig::wiki(text, caret);
        rig.overlays.wikilink_mut().open(Rect::ZERO, "a");
        assert!(rig.overlays.wikilink_visible());
        rig.editor_event(EditorEvent::Edited);
        assert!(!rig.overlays.wikilink_visible(), "{text:?}");
    }
}

#[test]
fn escape_dismisses_the_completion_and_the_text_stays() {
    let mut rig = Rig::wiki("see [[al", 8);
    rig.editor_event(EditorEvent::Edited);
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.wikilink_visible());
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "see [[al");
}

// ----- pump: find, go to, Quick Open, find highlights ------------------------------------------

#[test]
fn the_find_bar_moves_the_editor_replaces_and_closes() {
    let mut rig = Rig::new("one two one\n");
    rig.overlays.open_find(true);
    rig.overlays.find_mut().set_pattern("one");
    // The editor was elsewhere: the reveal brings it to the match.
    rig.doc
        .borrow_mut()
        .set_selections(Selection::caret(5), &[]);
    assert!(rig.pump().is_empty());
    assert_eq!(rig.editor.selection(), Selection::new(0, 3));
    rig.overlays.find_mut().set_replacement("1");
    assert_eq!(rig.overlays.find_mut().replace_all(), 2);
    assert!(rig.pump().is_empty());
    assert_eq!(rig.text(), "1 two 1\n");
    rig.overlays.find_mut().close();
    assert!(rig.pump().is_empty());
    assert!(rig.overlays.take_refocus());
}

#[test]
fn find_highlights_follow_the_bar_and_vanish_when_it_closes() {
    let mut rig = Rig::new("ab\n\nab\n");
    let palette = &rig.theme.tokens.highlight;
    let (current, other) = (palette.orange.background, palette.yellow.background);
    rig.overlays.open_find(false);
    rig.overlays.find_mut().set_pattern("ab");
    rig.pump();
    assert_eq!(rig.highlight_fills(), [current, other]);
    rig.overlays.find_mut().close();
    rig.pump();
    assert!(rig.highlight_fills().is_empty());
}

#[test]
fn find_highlights_follow_the_current_match_and_edits() {
    let mut rig = Rig::new("ab

ab
");
    let palette = &rig.theme.tokens.highlight;
    let (current, other) = (palette.orange.background, palette.yellow.background);
    rig.overlays.open_find(false);
    rig.overlays.find_mut().set_pattern("ab");
    rig.pump();
    rig.pump();
    assert_eq!(rig.highlight_fills(), [current, other]);
    assert!(rig.overlays.find_mut().find_next());
    rig.pump();
    assert_eq!(rig.highlight_fills(), [other, current]);
    let before = rig.doc.borrow().primary_selection();
    rig.doc
        .borrow_mut()
        .apply(Transaction::single(
            Edit::insert(0, "ab "),
            before,
            Selection::caret(3),
        ))
        .unwrap();
    rig.pump();
    assert_eq!(rig.highlight_fills(), [other, current, other]);
}

#[test]
fn closing_find_after_a_tab_switch_leaves_no_highlights_in_the_tab_left_behind() {
    let mut rig = Rig::new("ab

ab
");
    let doc_a = Rc::clone(&rig.doc);
    let doc_b = Rc::new(RefCell::new(Document::new("xx ab
")));
    let clock: Rc<dyn Clock> = Rc::new(FakeClock::default());
    let mut tab_b = EditorView::new(Rc::clone(&doc_b), clock);
    rig.overlays.open_find(false);
    rig.overlays.find_mut().set_pattern("ab");
    rig.pump();
    assert_eq!(rig.highlight_fills().len(), 2);

    std::mem::swap(&mut rig.editor, &mut tab_b);
    rig.overlays.set_document(&doc_b);
    rig.layout();
    rig.pump();
    assert_eq!(rig.highlight_fills().len(), 1, "the match of the new tab");

    rig.overlays.find_mut().close();
    rig.pump();
    assert!(rig.highlight_fills().is_empty());

    std::mem::swap(&mut rig.editor, &mut tab_b);
    rig.overlays.set_document(&doc_a);
    rig.pump();
    assert!(
        rig.highlight_fills().is_empty(),
        "the first tab still painted the matches of the closed bar"
    );
}

#[test]
fn go_to_line_moves_the_editor_and_quick_open_goes_to_headings_or_closes() {
    let mut rig = Rig::new("alpha\nbeta\ngamma\n");
    rig.overlays.open_goto();
    rig.layout();
    assert!(rig.deliver(typed("3:2")).handled);
    assert!(rig.press(vk::RETURN).handled);
    rig.doc
        .borrow_mut()
        .set_selections(Selection::caret(0), &[]);
    assert!(rig.pump().is_empty());
    assert_eq!(rig.editor.selection(), Selection::caret(12));
    assert!(rig.overlays.take_refocus());

    rig.overlays.open_goto();
    rig.overlays.goto_mut().close();
    assert!(rig.pump().is_empty());
    assert!(rig.overlays.take_refocus());

    let mut rig = Rig::new("# One\n\ntext\n\n## Two\n");
    let toc = rig.toc();
    rig.overlays
        .open_quick_open(QuickOpenMode::Headings, Vec::new(), Vec::new(), &toc);
    rig.layout();
    assert!(rig.deliver(typed("two")).handled);
    assert!(rig.press(vk::RETURN).handled);
    assert!(rig.pump().is_empty());
    assert_eq!(
        rig.editor.selection(),
        Selection::caret(toc.entries()[1].range.start)
    );

    rig.overlays.open_quick_open(
        QuickOpenMode::Files,
        vec!["a.md".to_owned()],
        Vec::new(),
        &toc,
    );
    rig.layout();
    assert!(rig.press(vk::ESCAPE).handled);
    assert!(!rig.overlays.quick_open_visible());
    assert!(rig.pump().is_empty());
    assert!(rig.overlays.take_refocus());
}

#[test]
fn without_an_active_editor_the_overlay_events_are_consumed_and_change_nothing() {
    let mut rig = Rig::wiki("# One\n\ntext\n", 0);
    let before = rig.text();
    rig.overlays.open_find(false);
    rig.overlays.find_mut().set_pattern("text");
    rig.overlays.open_goto();
    rig.overlays.goto_mut().set_input("2");
    assert!(rig.overlays.goto_mut().submit());
    let toc = rig.toc();
    rig.overlays
        .open_quick_open(QuickOpenMode::Headings, Vec::new(), Vec::new(), &toc);
    rig.overlays.quick_open_mut().set_input("#one");
    assert!(rig.overlays.quick_open_mut().activate());
    rig.overlays
        .link_mut()
        .show(Rect::ZERO, "text", "https://example.org");
    rig.overlays.link_mut().apply();
    rig.overlays.wikilink_mut().open(Rect::ZERO, "al");
    assert!(rig.overlays.wikilink_mut().accept());
    rig.overlays.find_mut().close();

    assert!(rig.overlays.pump(None).is_empty());
    assert!(rig.overlays.take_refocus());
    assert_eq!(rig.text(), before);
    assert!(rig.overlays.find_mut().take_events().is_empty());
    assert!(rig.overlays.link_mut().take_events().is_empty());
    assert!(rig.overlays.wikilink_mut().take_events().is_empty());
}
