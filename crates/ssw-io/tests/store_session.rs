//! Session restore, recent items, per-file view state and untitled buffer backups.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use ssw_io::session::{
    RECENT_LIMIT, RecentItem, RecentKind, Session, SessionTab, SessionWindow, TabDocument,
    ViewState, WindowPlacement,
};
use ssw_io::store::{Store, StoreError};
use tempfile::{TempDir, tempdir};

fn open() -> (TempDir, Store, PathBuf) {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let store = Store::open(&path).unwrap();
    (tmp, store, path)
}

fn file_tab(path: &str, workspace: Option<&str>) -> SessionTab {
    SessionTab {
        document: TabDocument::File(PathBuf::from(path)),
        workspace: workspace.map(PathBuf::from),
    }
}

fn untitled_tab(id: &str) -> SessionTab {
    SessionTab {
        document: TabDocument::Untitled(id.to_owned()),
        workspace: None,
    }
}

fn placement(x: i32, maximized: bool) -> WindowPlacement {
    WindowPlacement {
        x,
        y: -20,
        width: 1280,
        height: 720,
        maximized,
    }
}

fn sample_session() -> Session {
    Session {
        windows: vec![
            SessionWindow {
                placement: placement(-1920, false),
                tabs: vec![
                    file_tab("C:\\notes\\a.md", Some("C:\\notes")),
                    untitled_tab("untitled-1"),
                    file_tab("D:\\other\\ünal 🙂.txt", None),
                ],
                active_tab: Some(1),
            },
            SessionWindow {
                placement: placement(100, true),
                tabs: vec![file_tab("C:\\notes\\b.md", Some("C:\\notes"))],
                active_tab: None,
            },
            SessionWindow {
                placement: placement(200, false),
                tabs: Vec::new(),
                active_tab: None,
            },
        ],
        active_window: Some(1),
    }
}

fn count(path: &Path, table: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn a_new_database_has_an_empty_session() {
    let (_tmp, store, _) = open();
    assert_eq!(store.load_session().unwrap(), Session::default());
}

#[test]
fn session_survives_a_restart_exactly() {
    let (_tmp, mut store, path) = open();
    store.save_session(&sample_session()).unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    assert_eq!(store.load_session().unwrap(), sample_session());
}

#[test]
fn saving_replaces_the_previous_session_and_its_tabs() {
    let (_tmp, mut store, path) = open();
    store.save_session(&sample_session()).unwrap();
    assert_eq!(count(&path, "session_windows"), 3);
    assert_eq!(count(&path, "session_tabs"), 4);

    let smaller = Session {
        windows: vec![SessionWindow {
            placement: placement(5, false),
            tabs: vec![file_tab("C:\\x.md", None)],
            active_tab: Some(0),
        }],
        active_window: None,
    };
    store.save_session(&smaller).unwrap();
    assert_eq!(store.load_session().unwrap(), smaller);
    assert_eq!(count(&path, "session_windows"), 1);
    // The tabs of the deleted windows went away with them (foreign key cascade).
    assert_eq!(count(&path, "session_tabs"), 1);

    store.save_session(&Session::default()).unwrap();
    assert_eq!(store.load_session().unwrap(), Session::default());
    assert_eq!(count(&path, "session_tabs"), 0);
}

#[test]
fn a_session_with_an_unrepresentable_path_is_rejected_and_changes_nothing() {
    let (_tmp, mut store, _) = open();
    store.save_session(&sample_session()).unwrap();

    let bad = PathBuf::from(OsString::from_wide(&[0x43, 0x3A, 0x5C, 0xD800]));
    for tab in [
        SessionTab {
            document: TabDocument::File(bad.clone()),
            workspace: None,
        },
        SessionTab {
            document: TabDocument::File(PathBuf::from("C:\\ok.md")),
            workspace: Some(bad.clone()),
        },
    ] {
        let broken = Session {
            windows: vec![SessionWindow {
                placement: placement(0, false),
                tabs: vec![tab],
                active_tab: None,
            }],
            active_window: None,
        };
        let error = store.save_session(&broken).unwrap_err();
        assert!(matches!(error, StoreError::NonUnicodePath(_)), "{error:?}");
        assert_eq!(store.load_session().unwrap(), sample_session());
    }
}

#[test]
fn first_active_flag_wins_when_several_rows_claim_it() {
    let (_tmp, store, path) = open();
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch(
        "INSERT INTO session_windows (id, x, y, width, height, maximized, active)
         VALUES (0, 0, 0, 800, 600, 0, 1), (1, 0, 0, 800, 600, 0, 1);
         INSERT INTO session_tabs (window_id, position, path, untitled_id, workspace, active)
         VALUES (0, 0, 'C:\\a.md', NULL, NULL, 1), (0, 1, 'C:\\b.md', NULL, NULL, 1);",
    )
    .unwrap();
    let session = store.load_session().unwrap();
    assert_eq!(session.active_window, Some(0));
    assert_eq!(session.windows[0].active_tab, Some(0));
    assert_eq!(session.windows[1].active_tab, None);
}

#[test]
fn a_tab_row_without_a_document_is_invalid_data() {
    let (_tmp, store, path) = open();
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "INSERT INTO session_windows (id, x, y, width, height, maximized, active)
             VALUES (0, 0, 0, 800, 600, 0, 0);
             INSERT INTO session_tabs (window_id, position, path, untitled_id, workspace, active)
             VALUES (0, 0, NULL, NULL, NULL, 0);",
        )
        .unwrap();
    let error = store.load_session().unwrap_err();
    assert!(matches!(error, StoreError::InvalidData(_)), "{error:?}");
}

#[test]
fn untitled_buffers_are_backed_up_replaced_and_deleted() {
    let (_tmp, store, _) = open();
    assert_eq!(store.untitled_get("u1").unwrap(), None);
    store.untitled_put("u1", "first draft", 10).unwrap();
    store.untitled_put("u1", "second draft ünal", 20).unwrap();
    assert_eq!(
        store.untitled_get("u1").unwrap().as_deref(),
        Some("second draft ünal")
    );
    store.untitled_delete("u1").unwrap();
    assert_eq!(store.untitled_get("u1").unwrap(), None);
    // Deleting a missing buffer is not an error.
    store.untitled_delete("u1").unwrap();
}

#[test]
fn saving_the_session_drops_backups_no_tab_refers_to() {
    let (_tmp, mut store, _) = open();
    store.untitled_put("untitled-1", "kept", 1).unwrap();
    store.untitled_put("gone", "dropped", 1).unwrap();
    store.save_session(&sample_session()).unwrap();
    assert_eq!(
        store.untitled_get("untitled-1").unwrap().as_deref(),
        Some("kept")
    );
    assert_eq!(store.untitled_get("gone").unwrap(), None);
}

#[test]
fn recent_files_keep_the_newest_twenty_most_recent_first() {
    let (_tmp, mut store, _) = open();
    assert!(store.recent_items(RecentKind::File).unwrap().is_empty());
    for i in 0..25 {
        let path = PathBuf::from(format!("C:\\docs\\f{i}.md"));
        store.recent_add(RecentKind::File, &path, 1000 + i).unwrap();
    }
    let items = store.recent_items(RecentKind::File).unwrap();
    assert_eq!(items.len(), RECENT_LIMIT);
    assert_eq!(
        items[0],
        RecentItem {
            path: PathBuf::from("C:\\docs\\f24.md"),
            opened_at: 1024
        }
    );
    assert_eq!(items[19].path, PathBuf::from("C:\\docs\\f5.md"));
}

#[test]
fn opening_a_recent_file_again_moves_it_to_the_front_without_duplicates() {
    let (_tmp, mut store, _) = open();
    for (i, name) in ["a", "b", "c"].iter().enumerate() {
        let path = PathBuf::from(format!("C:\\docs\\{name}.md"));
        store.recent_add(RecentKind::File, &path, i as i64).unwrap();
    }
    // Same file, different spelling.
    store
        .recent_add(RecentKind::File, Path::new("c:/DOCS/A.MD"), 99)
        .unwrap();
    let items = store.recent_items(RecentKind::File).unwrap();
    assert_eq!(
        items,
        vec![
            RecentItem {
                path: PathBuf::from("c:/DOCS/A.MD"),
                opened_at: 99
            },
            RecentItem {
                path: PathBuf::from("C:\\docs\\c.md"),
                opened_at: 2
            },
            RecentItem {
                path: PathBuf::from("C:\\docs\\b.md"),
                opened_at: 1
            },
        ]
    );
}

#[test]
fn recent_files_and_folders_are_separate_lists() {
    let (_tmp, mut store, _) = open();
    let same = Path::new("C:\\work");
    store.recent_add(RecentKind::File, same, 1).unwrap();
    store.recent_add(RecentKind::Folder, same, 2).unwrap();
    store
        .recent_add(RecentKind::Folder, Path::new("C:\\other"), 3)
        .unwrap();
    assert_eq!(store.recent_items(RecentKind::File).unwrap().len(), 1);
    assert_eq!(store.recent_items(RecentKind::Folder).unwrap().len(), 2);

    // Removal is case-insensitive and only touches the named kind.
    store
        .recent_remove(RecentKind::Folder, Path::new("c:\\WORK"))
        .unwrap();
    let folders = store.recent_items(RecentKind::Folder).unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].path, PathBuf::from("C:\\other"));
    assert_eq!(store.recent_items(RecentKind::File).unwrap().len(), 1);

    store.recent_clear(RecentKind::Folder).unwrap();
    assert!(store.recent_items(RecentKind::Folder).unwrap().is_empty());
    assert_eq!(store.recent_items(RecentKind::File).unwrap().len(), 1);
}

#[test]
fn recent_items_reject_unrepresentable_paths() {
    let (_tmp, mut store, _) = open();
    let bad = PathBuf::from(OsString::from_wide(&[0x41, 0xDC00]));
    let add = store.recent_add(RecentKind::File, &bad, 1).unwrap_err();
    assert!(matches!(add, StoreError::NonUnicodePath(_)));
    let remove = store.recent_remove(RecentKind::File, &bad).unwrap_err();
    assert!(matches!(remove, StoreError::NonUnicodePath(_)));
    assert!(store.recent_items(RecentKind::File).unwrap().is_empty());
}

#[test]
fn view_state_round_trips_and_is_found_case_insensitively() {
    let (_tmp, store, path) = open();
    let file = Path::new("C:\\Notes\\Doc.md");
    assert_eq!(store.view_state(file).unwrap(), None);

    let state = ViewState {
        caret: 1234,
        scroll_anchor: 1000,
        scroll_delta: 12.5,
        folds: vec![0, 120, 4096],
    };
    store.view_state_put(file, &state, 5).unwrap();
    assert_eq!(store.view_state(file).unwrap(), Some(state.clone()));
    assert_eq!(
        store.view_state(Path::new("c:/notes/DOC.md")).unwrap(),
        Some(state)
    );

    let replaced = ViewState {
        caret: 7,
        scroll_anchor: 0,
        scroll_delta: -3.25,
        folds: Vec::new(),
    };
    store
        .view_state_put(Path::new("c:/notes/doc.MD"), &replaced, 6)
        .unwrap();
    assert_eq!(store.view_state(file).unwrap(), Some(replaced));
    assert_eq!(count(&path, "file_view_state"), 1);

    store.view_state_forget(file).unwrap();
    assert_eq!(store.view_state(file).unwrap(), None);
    // Forgetting an unknown file is not an error.
    store.view_state_forget(file).unwrap();
}

#[test]
fn view_state_with_damaged_folds_is_a_json_error() {
    let (_tmp, store, path) = open();
    let file = Path::new("C:\\Notes\\Doc.md");
    store
        .view_state_put(file, &ViewState::default(), 1)
        .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE file_view_state SET folds = 'not json'", [])
        .unwrap();
    let error = store.view_state(file).unwrap_err();
    assert!(matches!(error, StoreError::Json(_)), "{error:?}");
}

#[test]
fn view_state_rejects_unrepresentable_paths() {
    let (_tmp, store, _) = open();
    let bad = PathBuf::from(OsString::from_wide(&[0x41, 0xD800]));
    assert!(matches!(
        store
            .view_state_put(&bad, &ViewState::default(), 1)
            .unwrap_err(),
        StoreError::NonUnicodePath(_)
    ));
    assert!(matches!(
        store.view_state(&bad).unwrap_err(),
        StoreError::NonUnicodePath(_)
    ));
    assert!(matches!(
        store.view_state_forget(&bad).unwrap_err(),
        StoreError::NonUnicodePath(_)
    ));
}
