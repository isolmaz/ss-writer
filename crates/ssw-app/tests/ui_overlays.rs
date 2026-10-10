//! Behavior of the overlays (spec §7): find / replace on a real document, Go to line, the
//! command palette, folder search requests and the workspace file list Quick Open shows.

use std::cell::RefCell;
use std::rc::Rc;

use ssw_app::i18n::localizer_for;
use ssw_app::ui::overlays::{
    NoShortcuts, OverlayAction, Overlays, list_workspace_files, panel_files,
};
use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;
use ssw_io::folder_search::{FileMatches, LineMatch};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::shell::quick_open::QuickOpenMode;
use ssw_ui::widgets::SharedClipboard;

fn overlays_over(text: &str) -> (Overlays, Rc<RefCell<Document>>) {
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    let localizer = Rc::new(localizer_for("en"));
    let mut overlays = Overlays::new(&localizer, &clipboard, &NoShortcuts);
    let doc = Rc::new(RefCell::new(Document::new(text)));
    overlays.set_document(&doc);
    (overlays, doc)
}

#[test]
fn find_selects_matches_and_replace_all_is_one_undo_step() {
    let (mut overlays, doc) = overlays_over("one two one\n");
    overlays.open_find(true);
    assert!(overlays.find_visible());
    overlays.find_mut().set_pattern("one");
    assert_eq!(
        doc.borrow().primary_selection().range(),
        TextRange::new(0, 3)
    );
    overlays.find_next();
    assert_eq!(
        doc.borrow().primary_selection().range(),
        TextRange::new(8, 11)
    );
    overlays.find_previous();
    assert_eq!(
        doc.borrow().primary_selection().range(),
        TextRange::new(0, 3)
    );

    overlays.find_mut().set_replacement("X");
    assert_eq!(overlays.find_mut().replace_all(), 2);
    assert_eq!(doc.borrow().text(), "X two X\n");
    assert!(doc.borrow_mut().undo().unwrap());
    assert_eq!(doc.borrow().text(), "one two one\n");
}

#[test]
fn go_to_line_moves_the_caret_to_the_line_and_column() {
    let (mut overlays, doc) = overlays_over("alpha\nbeta\ngamma\n");
    overlays.open_goto();
    assert!(overlays.goto_visible());
    overlays.goto_mut().set_input("2:3");
    assert!(overlays.goto_mut().submit());
    assert!(!overlays.goto_visible());
    assert_eq!(doc.borrow().primary_selection().head, "alpha\nbe".len());
}

#[test]
fn the_palette_runs_the_chosen_command() {
    let (mut overlays, _doc) = overlays_over("text\n");
    overlays.open_quick_open(
        QuickOpenMode::Commands,
        Vec::new(),
        Vec::new(),
        &Toc::default(),
    );
    assert!(overlays.quick_open_visible());
    overlays.quick_open_mut().set_input(">Bold");
    assert!(overlays.quick_open_mut().activate());
    assert!(!overlays.quick_open_visible());
    assert_eq!(
        overlays.pump(None),
        vec![OverlayAction::RunCommand(CommandId::FormatBold)]
    );
}

#[test]
fn quick_open_reports_the_chosen_file() {
    let (mut overlays, _doc) = overlays_over("text\n");
    let files = vec!["notes/alpha.md".to_owned(), "beta.md".to_owned()];
    overlays.open_quick_open(QuickOpenMode::Files, files, Vec::new(), &Toc::default());
    overlays.quick_open_mut().set_input("beta");
    assert!(overlays.quick_open_mut().activate());
    assert_eq!(
        overlays.pump(None),
        vec![OverlayAction::OpenFile("beta.md".to_owned())]
    );
}

#[test]
fn folder_search_asks_the_controller_to_search() {
    let (mut overlays, _doc) = overlays_over("text\n");
    overlays.open_folder_search(Some("needle"));
    assert!(overlays.folder_search_visible());
    assert!(overlays.folder_search_mut().submit());
    let actions = overlays.pump(None);
    let [OverlayAction::FolderSearch(request)] = actions.as_slice() else {
        panic!("expected one search request, got {actions:?}");
    };
    assert_eq!(request.pattern, "needle");
    assert!(!request.regex);
    overlays.close_folder_search();
    assert!(!overlays.folder_search_visible());
}

#[test]
fn the_workspace_file_list_is_relative_sorted_and_skips_hidden_and_git() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".private")).unwrap();
    std::fs::write(root.join("b.md"), "b").unwrap();
    std::fs::write(root.join("sub").join("a.md"), "a").unwrap();
    std::fs::write(root.join(".git").join("HEAD"), "ref").unwrap();
    std::fs::write(root.join(".private").join("c.md"), "c").unwrap();
    assert_eq!(list_workspace_files(root, false), ["b.md", "sub/a.md"]);
    assert_eq!(
        list_workspace_files(root, true),
        [".private/c.md", "b.md", "sub/a.md"]
    );
}

#[test]
fn search_results_become_panel_rows_relative_to_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let found = vec![FileMatches {
        path: root.join("sub").join("b.md"),
        matches: vec![LineMatch {
            line: 3,
            column: 2,
            ranges: vec![2..8],
            preview: "a needle here".to_owned(),
        }],
    }];
    let files = panel_files(root, found);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "sub/b.md");
    assert_eq!(files[0].matches[0].line, 3);
    assert_eq!(files[0].matches[0].ranges, vec![2..8]);
    assert_eq!(files[0].matches[0].preview, "a needle here");
}
