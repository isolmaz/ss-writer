//! Database failures a healthy machine never produces. A second connection drops tables,
//! recreates them without a column, damages stored values or installs triggers that abort one
//! kind of statement; every `Store` operation must then report a `StoreError::Sqlite` that names
//! the cause, and an operation made of several statements must leave the previous state intact.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use rusqlite::Connection;
use ssw_io::history::{SnapshotOutcome, SnapshotReason};
use ssw_io::index::{IndexChange, IndexOptions, IndexStats};
use ssw_io::session::{
    RECENT_LIMIT, RecentKind, Session, SessionTab, SessionWindow, TabDocument, ViewState,
    WindowPlacement,
};
use ssw_io::store::{Store, StoreError};
use tempfile::{TempDir, tempdir};

const NOW: i64 = 1_700_000_000;
const DAY: i64 = 24 * 60 * 60;

/// A store and a second connection to the same database file.
struct Db {
    _tmp: TempDir,
    store: Store,
    raw: Connection,
}

fn db() -> Db {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let store = Store::open(&path).unwrap();
    let raw = Connection::open(&path).unwrap();
    // Tables are dropped and recreated without regard for the references between them.
    raw.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    Db {
        _tmp: tmp,
        store,
        raw,
    }
}

impl Db {
    fn sql(&self, sql: &str) {
        self.raw.execute_batch(sql).unwrap();
    }

    /// Statements of kind `event` (`INSERT`, `UPDATE` or `DELETE`) on `table` abort.
    fn abort(&self, event: &str, table: &str) {
        self.sql(&format!(
            "CREATE TRIGGER abort_{event}_{table} BEFORE {event} ON {table}
             BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
        ));
    }

    fn count(&self, table: &str) -> i64 {
        self.raw
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

/// The statement that makes every insert into `table` abort.
fn abort_insert(table: &str) -> String {
    format!(
        "CREATE TRIGGER abort_insert BEFORE INSERT ON {table}
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
    )
}

/// The message of the database error `result` failed with.
fn message<T: Debug>(result: Result<T, StoreError>) -> String {
    match result.unwrap_err() {
        StoreError::Sqlite(error) => error.to_string(),
        other => panic!("not a database error: {other:?}"),
    }
}

/// Asserts that `result` is a database error whose message contains `expected`.
fn fails_with<T: Debug>(result: Result<T, StoreError>, expected: &str) {
    let text = message(result);
    assert!(text.contains(expected), "{expected:?} not in {text:?}");
}

// ---------------------------------------------------------------------------------------------
// Opening and the key/value table
// ---------------------------------------------------------------------------------------------

#[test]
fn a_migration_that_fails_is_reported_and_rolled_back() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    {
        // The last table of migration 1 exists although the schema version says that no
        // migration ran.
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
    }
    fails_with(Store::open(&path), "already exists");

    let conn = Connection::open(&path).unwrap();
    let first_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'session_windows'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!((first_table, version), (0, 0));
}

#[test]
fn key_value_access_reports_a_missing_table() {
    let d = db();
    d.sql("DROP TABLE kv");
    fails_with(d.store.kv_get("k"), "no such table");
    fails_with(d.store.kv_set("k", "v"), "no such table");
}

// ---------------------------------------------------------------------------------------------
// Version history
// ---------------------------------------------------------------------------------------------

const NOTE: &str = "C:\\notes\\a.md";

fn created(outcome: SnapshotOutcome) -> i64 {
    match outcome {
        SnapshotOutcome::Created(id) => id,
        SnapshotOutcome::Unchanged(id) => panic!("expected a new snapshot, got unchanged {id}"),
    }
}

#[test]
fn history_operations_report_a_missing_snapshot_table() {
    let mut d = db();
    let path = Path::new(NOTE);
    d.sql("DROP TABLE history_snapshots");
    fails_with(
        d.store
            .history_snapshot(path, "text", SnapshotReason::Close, 1),
        "no such table",
    );
    fails_with(d.store.history_load(1), "no such table");
    fails_with(
        d.store.history_restore(path, 1, "current", 2),
        "no such table",
    );
}

#[test]
fn a_snapshot_whose_insert_fails_leaves_no_blob_behind() {
    for table in ["history_blobs", "history_snapshots"] {
        let mut d = db();
        d.abort("INSERT", table);
        fails_with(
            d.store
                .history_snapshot(Path::new(NOTE), "text", SnapshotReason::Close, 1),
            "injected failure",
        );
        assert_eq!(
            (d.count("history_blobs"), d.count("history_snapshots")),
            (0, 0),
            "{table}"
        );
    }
}

#[test]
fn restoring_reports_a_failure_to_save_the_current_text() {
    let mut d = db();
    let path = Path::new(NOTE);
    let id = created(
        d.store
            .history_snapshot(path, "old", SnapshotReason::Close, 1)
            .unwrap(),
    );
    d.abort("INSERT", "history_snapshots");
    fails_with(
        d.store.history_restore(path, id, "current", 2),
        "injected failure",
    );
    assert_eq!(d.count("history_snapshots"), 1);
}

#[test]
fn a_purge_that_fails_deletes_nothing() {
    for table in ["history_snapshots", "history_blobs"] {
        let mut d = db();
        d.store
            .history_snapshot(Path::new(NOTE), "old", SnapshotReason::Close, 1)
            .unwrap();
        d.abort("DELETE", table);
        fails_with(d.store.history_purge(100 * DAY, 1), "injected failure");
        // The snapshot and its blob are deleted together or not at all.
        assert_eq!(
            (d.count("history_snapshots"), d.count("history_blobs")),
            (1, 1),
            "{table}"
        );
    }
}

#[test]
fn history_values_of_the_wrong_type_are_database_errors() {
    let path = Path::new(NOTE);

    let mut d = db();
    d.store
        .history_snapshot(path, "text", SnapshotReason::Close, 1)
        .unwrap();
    d.sql("UPDATE history_snapshots SET created_at = 'yesterday'");
    fails_with(d.store.history_list(path), "Invalid column type");

    let mut d = db();
    let id = created(
        d.store
            .history_snapshot(path, "text", SnapshotReason::Close, 1)
            .unwrap(),
    );
    // The text sits in the blob column as TEXT instead of BLOB.
    d.sql("UPDATE history_blobs SET content = 'text'");
    fails_with(d.store.history_load(id), "Invalid column type");
}

// ---------------------------------------------------------------------------------------------
// Session, recent items, view state and untitled backups
// ---------------------------------------------------------------------------------------------

fn session(x: i32) -> Session {
    Session {
        windows: vec![SessionWindow {
            placement: WindowPlacement {
                x,
                y: 2,
                width: 300,
                height: 200,
                maximized: false,
            },
            tabs: vec![SessionTab {
                document: TabDocument::File(PathBuf::from("C:\\docs\\a.md")),
                workspace: None,
            }],
            active_tab: Some(0),
        }],
        active_window: Some(0),
    }
}

#[test]
fn a_failing_session_save_keeps_the_previous_session() {
    // (breakage, message, whether the stored session can still be loaded afterwards)
    let cases = [
        (
            "DROP TABLE session_windows".to_owned(),
            "no such table",
            false,
        ),
        (abort_insert("session_windows"), "injected failure", true),
        (abort_insert("session_tabs"), "injected failure", true),
        (
            "DROP TABLE untitled_buffers".to_owned(),
            "no such table",
            true,
        ),
    ];
    for (breakage, expected, loadable) in &cases {
        let mut d = db();
        d.store.save_session(&session(1)).unwrap();
        d.sql(breakage);
        fails_with(d.store.save_session(&session(99)), expected);
        if *loadable {
            assert_eq!(d.store.load_session().unwrap(), session(1), "{breakage}");
        }
    }
}

#[test]
fn loading_a_session_reports_missing_tables_and_damaged_values() {
    let d = db();
    d.sql("DROP TABLE session_windows");
    fails_with(d.store.load_session(), "no such table");

    // The tabs are only read for a window that exists.
    let mut d = db();
    d.store.save_session(&session(1)).unwrap();
    d.sql("DROP TABLE session_tabs");
    fails_with(d.store.load_session(), "no such table");

    let mut d = db();
    d.store.save_session(&session(1)).unwrap();
    d.sql("UPDATE session_windows SET x = 'left'");
    fails_with(d.store.load_session(), "Invalid column type");

    let mut d = db();
    d.store.save_session(&session(1)).unwrap();
    d.sql("UPDATE session_tabs SET active = 'yes'");
    fails_with(d.store.load_session(), "Invalid column type");
}

#[test]
fn recent_items_report_a_missing_table() {
    let mut d = db();
    let path = Path::new("C:\\docs\\a.md");
    d.sql("DROP TABLE recent_items");
    fails_with(
        d.store.recent_add(RecentKind::File, path, 1),
        "no such table",
    );
    fails_with(d.store.recent_items(RecentKind::File), "no such table");
    fails_with(
        d.store.recent_remove(RecentKind::File, path),
        "no such table",
    );
    fails_with(d.store.recent_clear(RecentKind::File), "no such table");
}

#[test]
fn adding_a_recent_item_stops_at_the_failing_statement() {
    let path = Path::new("C:\\docs\\a.md");

    // Replacing an item that is listed already: its old entry cannot be deleted.
    let mut d = db();
    d.store.recent_add(RecentKind::File, path, 1).unwrap();
    d.abort("DELETE", "recent_items");
    fails_with(
        d.store.recent_add(RecentKind::File, path, 2),
        "injected failure",
    );
    let listed = d.store.recent_items(RecentKind::File).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].opened_at, 1);

    // The new entry cannot be inserted.
    let mut d = db();
    d.abort("INSERT", "recent_items");
    fails_with(
        d.store.recent_add(RecentKind::File, path, 1),
        "injected failure",
    );
    assert!(d.store.recent_items(RecentKind::File).unwrap().is_empty());

    // The surplus beyond the limit cannot be trimmed: only the oldest entry (opened at 1) is
    // ever a victim, so only its deletion aborts.
    let mut d = db();
    d.sql(
        "CREATE TRIGGER keep_oldest BEFORE DELETE ON recent_items WHEN OLD.opened_at = 1
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END",
    );
    for i in 0..RECENT_LIMIT {
        let item = PathBuf::from(format!("C:\\docs\\{i}.md"));
        d.store
            .recent_add(RecentKind::File, &item, 1 + i as i64)
            .unwrap();
    }
    fails_with(
        d.store
            .recent_add(RecentKind::File, Path::new("C:\\docs\\newest.md"), 100),
        "injected failure",
    );
    assert_eq!(
        d.store.recent_items(RecentKind::File).unwrap().len(),
        RECENT_LIMIT
    );
}

#[test]
fn view_state_and_untitled_backups_report_missing_tables() {
    let d = db();
    let path = Path::new("C:\\docs\\a.md");
    d.sql("DROP TABLE file_view_state; DROP TABLE untitled_buffers");
    fails_with(
        d.store.view_state_put(path, &ViewState::default(), 1),
        "no such table",
    );
    fails_with(d.store.view_state(path), "no such table");
    fails_with(d.store.view_state_forget(path), "no such table");
    fails_with(d.store.untitled_put("u1", "text", 1), "no such table");
    fails_with(d.store.untitled_get("u1"), "no such table");
    fails_with(d.store.untitled_delete("u1"), "no such table");
}

#[test]
fn a_view_state_value_of_the_wrong_type_is_a_database_error() {
    let d = db();
    let path = Path::new("C:\\docs\\a.md");
    d.store
        .view_state_put(path, &ViewState::default(), 1)
        .unwrap();
    d.sql("UPDATE file_view_state SET caret = 'start'");
    fails_with(d.store.view_state(path), "Invalid column type");
}

// ---------------------------------------------------------------------------------------------
// Workspace index
// ---------------------------------------------------------------------------------------------

const DOC: &str = "# Title\n\nSee [other](other.md).\n";

fn no_placeholders(_: &Path) -> bool {
    false
}

/// A store, the second connection and a workspace folder with files.
struct Ix {
    db: Db,
    root: PathBuf,
}

fn ix(files: &[(&str, &str)]) -> Ix {
    let db = db();
    let root = db._tmp.path().join("ws");
    for (rel, text) in files {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    Ix { db, root }
}

/// A workspace that is indexed already.
fn indexed(files: &[(&str, &str)]) -> Ix {
    let mut ix = ix(files);
    ix.index().unwrap();
    ix
}

impl Ix {
    fn at(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn index(&mut self) -> Result<IndexStats, StoreError> {
        self.db.store.index_workspace(
            &self.root,
            &IndexOptions::default(),
            &no_placeholders,
            &AtomicBool::new(false),
            NOW,
        )
    }

    fn apply(&mut self, changes: &[IndexChange]) -> Result<IndexStats, StoreError> {
        self.db.store.index_apply_changes(
            &self.root,
            changes,
            &IndexOptions::default(),
            &no_placeholders,
        )
    }
}

/// `index_files` without its `markdown` column: a file cannot be inserted.
const FILES_WITHOUT_MARKDOWN: &str = "
    DROP TABLE index_files;
    CREATE TABLE index_files (
        id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL, rel TEXT NOT NULL,
        rel_key TEXT NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
        placeholder INTEGER NOT NULL, UNIQUE (root_id, rel_key)
    );";

/// `index_files` without the file name columns: files can be counted but not looked up.
const FILES_WITHOUT_NAMES: &str = "
    DROP TABLE index_files;
    CREATE TABLE index_files (id INTEGER PRIMARY KEY, root_id INTEGER NOT NULL);";

/// `index_headings` without its `text` column: a heading cannot be inserted (old ones can still
/// be deleted).
const HEADINGS_WITHOUT_TEXT: &str = "
    DROP TABLE index_headings;
    CREATE TABLE index_headings (
        file_id INTEGER NOT NULL, ord INTEGER NOT NULL, level INTEGER NOT NULL,
        pos INTEGER NOT NULL, PRIMARY KEY (file_id, ord)
    );";

/// `index_links` without its `heading` column: a link cannot be inserted.
const LINKS_WITHOUT_HEADING: &str = "
    DROP TABLE index_links;
    CREATE TABLE index_links (
        file_id INTEGER NOT NULL, ord INTEGER NOT NULL, wiki INTEGER NOT NULL,
        target TEXT NOT NULL, target_key TEXT NOT NULL, leaf_key TEXT NOT NULL,
        range_start INTEGER NOT NULL, range_end INTEGER NOT NULL, PRIMARY KEY (file_id, ord)
    );";

#[test]
fn indexing_reports_the_failing_statement() {
    let cases = [
        ("DROP TABLE index_roots".to_owned(), "no such table"),
        ("DROP TABLE index_files".to_owned(), "no such table"),
        (
            FILES_WITHOUT_MARKDOWN.to_owned(),
            "no column named markdown",
        ),
        (abort_insert("index_files"), "injected failure"),
        ("DROP TABLE index_headings".to_owned(), "no such table"),
        ("DROP TABLE index_links".to_owned(), "no such table"),
        (HEADINGS_WITHOUT_TEXT.to_owned(), "no column named text"),
        (LINKS_WITHOUT_HEADING.to_owned(), "no column named heading"),
        (abort_insert("index_headings"), "injected failure"),
        (abort_insert("index_links"), "injected failure"),
    ];
    for (breakage, expected) in &cases {
        let mut ix = ix(&[("doc.md", DOC), ("other.md", "# Other\n")]);
        ix.db.sql(breakage);
        let error = message(ix.index());
        assert!(error.contains(expected), "{breakage}: {error}");
    }
}

#[test]
fn a_failed_index_run_leaves_no_trace() {
    let mut ix = ix(&[("doc.md", DOC)]);
    ix.db.abort("INSERT", "index_links");
    fails_with(ix.index(), "injected failure");
    // The root, the file and its heading were written before the link failed.
    assert_eq!(
        (
            ix.db.count("index_roots"),
            ix.db.count("index_files"),
            ix.db.count("index_headings")
        ),
        (0, 0, 0)
    );
}

#[test]
fn re_indexing_a_changed_file_reports_a_failure_to_delete_its_old_rows() {
    for table in ["index_headings", "index_links"] {
        let mut ix = indexed(&[("doc.md", DOC)]);
        // A longer file is read again, and its old headings and links are deleted first.
        std::fs::write(ix.at("doc.md"), format!("{DOC}\nMore text.\n")).unwrap();
        ix.db.abort("DELETE", table);
        fails_with(ix.index(), "injected failure");
        assert_eq!(ix.db.count(table), 1, "{table}");
    }
}

#[test]
fn a_stale_file_that_cannot_be_dropped_keeps_the_whole_index() {
    let mut ix = indexed(&[("keep.md", "# Keep\n"), ("stale.md", "# Stale\n")]);
    std::fs::remove_file(ix.at("stale.md")).unwrap();
    ix.db.abort("DELETE", "index_files");
    fails_with(ix.index(), "injected failure");
    assert_eq!(ix.db.count("index_files"), 2);
}

#[test]
fn applying_changes_needs_the_root_and_file_tables() {
    for table in ["index_roots", "index_files"] {
        let mut ix = indexed(&[("doc.md", DOC)]);
        ix.db.sql(&format!("DROP TABLE {table}"));
        let doc = ix.at("doc.md");
        fails_with(ix.apply(&[IndexChange::Changed(doc)]), "no such table");
    }
}

#[test]
fn applying_changes_reports_a_failing_lookup_of_the_path() {
    // Files can still be counted, but not found by name.
    let ignoring = |ix: &Ix| std::fs::write(ix.at(".gitignore"), "doc.md\n").unwrap();

    // A changed file: is it known?
    let mut ix = indexed(&[("doc.md", DOC)]);
    ix.db.sql(FILES_WITHOUT_NAMES);
    let doc = ix.at("doc.md");
    fails_with(
        ix.apply(&[IndexChange::Changed(doc.clone())]),
        "no such column",
    );

    // A removed file.
    fails_with(
        ix.apply(&[IndexChange::Removed(doc.clone())]),
        "no such column",
    );

    // A changed file that the workspace ignores since: its entry is dropped.
    let mut ix = indexed(&[("doc.md", DOC)]);
    ignoring(&ix);
    ix.db.sql(FILES_WITHOUT_NAMES);
    let doc = ix.at("doc.md");
    fails_with(ix.apply(&[IndexChange::Changed(doc)]), "no such column");
}

#[test]
fn applying_changes_reports_a_failure_to_drop_entries_and_keeps_them() {
    let mut ix = indexed(&[("doc.md", DOC), ("dir/inner.md", "# Inner\n")]);
    ix.db.abort("DELETE", "index_files");
    let doc = ix.at("doc.md");

    // A removed file.
    fails_with(
        ix.apply(&[IndexChange::Removed(doc.clone())]),
        "injected failure",
    );
    // A removed folder: nothing is named like the folder itself, its content fails.
    let dir = ix.at("dir");
    fails_with(ix.apply(&[IndexChange::Removed(dir)]), "injected failure");
    // A changed file that the workspace ignores since.
    std::fs::write(ix.at(".gitignore"), "doc.md\n").unwrap();
    fails_with(ix.apply(&[IndexChange::Changed(doc)]), "injected failure");

    assert_eq!(ix.db.count("index_files"), 2);
}

#[test]
fn applying_changes_reports_a_failure_to_store_a_new_file() {
    let mut ix = indexed(&[("doc.md", DOC)]);
    std::fs::write(ix.at("new.md"), "# New\n").unwrap();
    ix.db.abort("INSERT", "index_files");
    let new = ix.at("new.md");
    fails_with(ix.apply(&[IndexChange::Changed(new)]), "injected failure");
    assert_eq!(ix.db.count("index_files"), 1);
}

#[test]
fn reading_the_index_reports_missing_tables() {
    let files = [("doc.md", DOC)];

    let ix = indexed(&files);
    ix.db.sql("DROP TABLE index_roots");
    let doc = ix.at("doc.md");
    let store = &ix.db.store;
    fails_with(store.index_forget(&ix.root), "no such table");
    fails_with(store.index_files(&ix.root), "no such table");
    fails_with(store.index_headings(&ix.root, &doc), "no such table");
    fails_with(store.index_links_to(&ix.root, &doc), "no such table");
    fails_with(store.index_backlinks(&ix.root, &doc), "no such table");

    let ix = indexed(&files);
    ix.db.sql("DROP TABLE index_files");
    fails_with(ix.db.store.index_files(&ix.root), "no such table");

    let ix = indexed(&files);
    ix.db.sql("DROP TABLE index_headings");
    fails_with(
        ix.db.store.index_headings(&ix.root, &ix.at("doc.md")),
        "no such table",
    );

    let ix = indexed(&files);
    ix.db.sql("DROP TABLE index_links");
    let doc = ix.at("doc.md");
    fails_with(ix.db.store.index_links_to(&ix.root, &doc), "no such table");
    fails_with(ix.db.store.index_backlinks(&ix.root, &doc), "no such table");
}
