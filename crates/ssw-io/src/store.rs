//! The SQLite state database (`state.db`): connection setup, versioned migrations, corruption
//! recovery and a small key/value table.
//!
//! The feature areas live in their own modules as further `impl Store` blocks: [`crate::session`]
//! (windows, tabs, recent items, per-file view state, untitled buffers), [`crate::history`]
//! (snapshots), [`crate::index`] (workspace index) and [`crate::remote_images`] (cache
//! metadata). Every timestamp is a Unix time supplied by the caller so tests inject the clock.

use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, OptionalExtension, Params, Row, Transaction};
use thiserror::Error;

/// Unix time in seconds.
pub type UnixSeconds = i64;

/// The settings of every connection: wait up to five seconds for a lock held by another
/// connection, write-ahead logging (readers never block the writer) with the relaxed `NORMAL`
/// sync mode that is safe with it, and enforced foreign keys.
const CONNECTION_PRAGMAS: &str = "
    PRAGMA busy_timeout = 5000;
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    PRAGMA foreign_keys = ON;
";

/// Ordered schema migrations: `MIGRATIONS[n]` upgrades a database at `user_version = n` to
/// `n + 1`. Released migrations are never edited; changes are appended.
pub const MIGRATIONS: &[&str] = &[
    // 1: session, recent items, per-file view state, untitled buffers, key/value.
    "
    CREATE TABLE session_windows (
        id INTEGER PRIMARY KEY,
        x INTEGER NOT NULL,
        y INTEGER NOT NULL,
        width INTEGER NOT NULL,
        height INTEGER NOT NULL,
        maximized INTEGER NOT NULL,
        active INTEGER NOT NULL
    );
    CREATE TABLE session_tabs (
        window_id INTEGER NOT NULL REFERENCES session_windows(id) ON DELETE CASCADE,
        position INTEGER NOT NULL,
        path TEXT,
        untitled_id TEXT,
        workspace TEXT,
        active INTEGER NOT NULL,
        PRIMARY KEY (window_id, position)
    );
    CREATE TABLE recent_items (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        kind INTEGER NOT NULL,
        key TEXT NOT NULL,
        path TEXT NOT NULL,
        opened_at INTEGER NOT NULL,
        UNIQUE (kind, key)
    );
    CREATE TABLE file_view_state (
        key TEXT PRIMARY KEY,
        path TEXT NOT NULL,
        caret INTEGER NOT NULL,
        scroll_anchor INTEGER NOT NULL,
        scroll_delta REAL NOT NULL,
        folds TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE untitled_buffers (
        id TEXT PRIMARY KEY,
        content TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE TABLE kv (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID;
    ",
    // 2: version history (content-addressed blobs shared between snapshots).
    "
    CREATE TABLE history_blobs (
        hash BLOB PRIMARY KEY,
        content BLOB NOT NULL
    ) WITHOUT ROWID;
    CREATE TABLE history_snapshots (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        key TEXT NOT NULL,
        path TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        reason INTEGER NOT NULL,
        hash BLOB NOT NULL REFERENCES history_blobs(hash)
    );
    CREATE INDEX history_snapshots_by_file ON history_snapshots(key, created_at);
    ",
    // 3: workspace index.
    "
    CREATE TABLE index_roots (
        id INTEGER PRIMARY KEY,
        key TEXT NOT NULL UNIQUE,
        path TEXT NOT NULL,
        indexed_at INTEGER NOT NULL
    );
    CREATE TABLE index_files (
        id INTEGER PRIMARY KEY,
        root_id INTEGER NOT NULL REFERENCES index_roots(id) ON DELETE CASCADE,
        rel TEXT NOT NULL,
        rel_key TEXT NOT NULL,
        size INTEGER NOT NULL,
        mtime INTEGER NOT NULL,
        markdown INTEGER NOT NULL,
        placeholder INTEGER NOT NULL,
        UNIQUE (root_id, rel_key)
    );
    CREATE TABLE index_headings (
        file_id INTEGER NOT NULL REFERENCES index_files(id) ON DELETE CASCADE,
        ord INTEGER NOT NULL,
        level INTEGER NOT NULL,
        text TEXT NOT NULL,
        pos INTEGER NOT NULL,
        PRIMARY KEY (file_id, ord)
    );
    CREATE TABLE index_links (
        file_id INTEGER NOT NULL REFERENCES index_files(id) ON DELETE CASCADE,
        ord INTEGER NOT NULL,
        wiki INTEGER NOT NULL,
        target TEXT NOT NULL,
        target_key TEXT NOT NULL,
        leaf_key TEXT NOT NULL,
        heading TEXT,
        range_start INTEGER NOT NULL,
        range_end INTEGER NOT NULL,
        PRIMARY KEY (file_id, ord)
    );
    CREATE INDEX index_links_by_leaf ON index_links(leaf_key);
    ",
    // 4: remote image cache metadata (the bytes live in the cache folder).
    "
    CREATE TABLE image_cache (
        key TEXT PRIMARY KEY,
        url TEXT NOT NULL,
        mime TEXT NOT NULL,
        size INTEGER NOT NULL,
        last_access INTEGER NOT NULL
    ) WITHOUT ROWID;
    CREATE INDEX image_cache_by_access ON image_cache(last_access);
    ",
];

/// The schema version produced by the newest migration.
#[must_use]
pub const fn latest_schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Failure of a state database operation.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("i/o error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "database schema version {found} is not supported (this version handles up to {supported})"
    )]
    UnsupportedSchema { found: i64, supported: i64 },
    #[error("path is not valid Unicode: {}", .0.display())]
    NonUnicodePath(PathBuf),
    #[error("stored data is invalid: {0}")]
    InvalidData(&'static str),
    #[error("stored JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("operation was cancelled")]
    Cancelled,
}

/// The state database.
#[derive(Debug)]
pub struct Store {
    pub(crate) conn: Connection,
    path: PathBuf,
    quarantined: Option<PathBuf>,
}

impl Store {
    /// Open (creating when missing) the database at `path`, migrating it to the latest schema.
    ///
    /// A file that is not a database, or is corrupt, is renamed aside to
    /// `<name>.corrupt[N]` (together with its `-wal` / `-shm` files) and a fresh database is
    /// created; [`Store::quarantined`] then names the moved file.
    ///
    /// # Errors
    /// [`StoreError::Io`] when the folder cannot be created or the corrupt file cannot be moved,
    /// [`StoreError::UnsupportedSchema`] for a database written by a newer version, and
    /// [`StoreError::Sqlite`] for any other database failure.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        // The folder of a bare file name is the empty path, which always exists; a drive root
        // or the empty path itself has no folder to create.
        let parent = path.parent().unwrap_or(Path::new(""));
        std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        match open_connection(path) {
            Ok(conn) => Ok(Self {
                conn,
                path: path.to_path_buf(),
                quarantined: None,
            }),
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)))
                if matches!(
                    failure.code,
                    ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt
                ) =>
            {
                // Whatever stops the fresh database from opening is reported as it is.
                quarantine(path).and_then(|aside| {
                    open_connection(path).map(|conn| Self {
                        conn,
                        path: path.to_path_buf(),
                        quarantined: Some(aside),
                    })
                })
            }
            Err(other) => Err(other),
        }
    }

    /// Run `body` in one transaction: it is committed when `body` returns `Ok` and rolled back
    /// when `body` or the commit fails, so a failed operation changes nothing.
    ///
    /// # Errors
    /// What `body` returns, and [`StoreError::Sqlite`] when the transaction cannot begin or
    /// commit.
    pub(crate) fn in_transaction<T>(
        &mut self,
        body: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        self.conn
            .transaction()
            .map_err(StoreError::from)
            .and_then(|tx| {
                body(&tx).and_then(|value| tx.commit().map(|()| value).map_err(StoreError::from))
            })
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the previous database was moved when it was corrupt at open time.
    #[must_use]
    pub fn quarantined(&self) -> Option<&Path> {
        self.quarantined.as_deref()
    }

    /// The schema version of the open database.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn schema_version(&self) -> Result<i64, StoreError> {
        user_version(&self.conn).map_err(StoreError::from)
    }

    /// Value stored under `key`.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Store `value` under `key`, replacing any previous value.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }
}

/// The schema version recorded in the database file.
fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// Every row of `sql` decoded as `T`, a tuple of column types (`(i64, String)`).
///
/// # Errors
/// [`StoreError::Sqlite`] when the statement cannot be prepared or run, or a column does not
/// have the type `T` asks for.
pub(crate) fn query_rows<T, P>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<T>, StoreError>
where
    P: Params,
    T: for<'r> TryFrom<&'r Row<'r>, Error = rusqlite::Error>,
{
    conn.prepare(sql)
        .and_then(|mut stmt| {
            stmt.query_map(params, |row| T::try_from(row))
                .and_then(|rows| rows.collect::<Result<Vec<T>, _>>())
        })
        .map_err(StoreError::from)
}

/// The first row of `sql` decoded as `T` (see [`query_rows`]); `None` when there is none.
///
/// # Errors
/// As [`query_rows`].
pub(crate) fn query_optional<T, P>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Option<T>, StoreError>
where
    P: Params,
    T: for<'r> TryFrom<&'r Row<'r>, Error = rusqlite::Error>,
{
    conn.query_row(sql, params, |row| T::try_from(row))
        .optional()
        .map_err(StoreError::from)
}

/// Connect, configure and migrate.
fn open_connection(path: &Path) -> Result<Connection, StoreError> {
    let mut conn = Connection::open(path)?;
    // The first statement that reads the file is where a file that is not a database fails.
    let version = conn
        .execute_batch(CONNECTION_PRAGMAS)
        .and_then(|()| user_version(&conn))?;
    migrate(&mut conn, version)?;
    Ok(conn)
}

/// Bring a database at schema `version` to the latest one, one transaction per migration.
fn migrate(conn: &mut Connection, version: i64) -> Result<(), StoreError> {
    if !(0..=latest_schema_version()).contains(&version) {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            supported: latest_schema_version(),
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        apply_migration(conn, index, sql)?;
    }
    Ok(())
}

/// Run migration number `index` and record the schema version it produces, atomically.
fn apply_migration(conn: &mut Connection, index: usize, sql: &str) -> rusqlite::Result<()> {
    conn.transaction().and_then(|tx| {
        tx.execute_batch(sql)
            .and_then(|()| tx.pragma_update(None, "user_version", index as i64 + 1))
            .and_then(|()| tx.commit())
    })
}

/// `path` with `suffix` appended to the whole file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Move the unusable database and its sidecar files out of the way; returns the new name of
/// the main file.
fn quarantine(path: &Path) -> Result<PathBuf, StoreError> {
    let mut counter = 0u32;
    let aside = loop {
        let suffix = if counter == 0 {
            ".corrupt".to_owned()
        } else {
            format!(".corrupt{counter}")
        };
        let candidate = with_suffix(path, &suffix);
        if !candidate.exists() {
            break candidate;
        }
        counter += 1;
    };
    for suffix in ["", "-wal", "-shm"] {
        let from = with_suffix(path, suffix);
        if suffix.is_empty() || from.exists() {
            let to = with_suffix(&aside, suffix);
            std::fs::rename(&from, &to).map_err(|source| StoreError::Io { path: from, source })?;
        }
    }
    Ok(aside)
}

/// `path` as UTF-8 text.
///
/// # Errors
/// [`StoreError::NonUnicodePath`] for paths with unpaired surrogates.
pub(crate) fn path_text(path: &Path) -> Result<&str, StoreError> {
    path.to_str()
        .ok_or_else(|| StoreError::NonUnicodePath(path.to_path_buf()))
}

/// The case-insensitive lookup key of a path text (Windows paths ignore case and accept `/`).
pub(crate) fn key_of(text: &str) -> String {
    text.replace('/', "\\").to_uppercase()
}

/// The case-insensitive lookup key of `path`.
///
/// # Errors
/// [`StoreError::NonUnicodePath`] for paths with unpaired surrogates.
pub fn path_key(path: &Path) -> Result<String, StoreError> {
    Ok(key_of(path_text(path)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn quarantine_moves_the_database_with_its_sidecar_files() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        write(&path, "main");
        write(&with_suffix(&path, "-wal"), "wal");
        write(&with_suffix(&path, "-shm"), "shm");

        let aside = quarantine(&path).unwrap();
        assert_eq!(aside, tmp.path().join("state.db.corrupt"));
        for (suffix, content) in [("", "main"), ("-wal", "wal"), ("-shm", "shm")] {
            assert_eq!(
                std::fs::read_to_string(with_suffix(&aside, suffix)).unwrap(),
                content
            );
            assert!(!with_suffix(&path, suffix).exists(), "{suffix} left behind");
        }
    }

    #[test]
    fn quarantine_without_sidecar_files_moves_just_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        write(&path, "main");
        write(&tmp.path().join("state.db.corrupt"), "an earlier one");

        let aside = quarantine(&path).unwrap();
        assert_eq!(aside, tmp.path().join("state.db.corrupt1"));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "main");
        assert!(!with_suffix(&aside, "-wal").exists());
        assert!(!with_suffix(&aside, "-shm").exists());
    }

    #[test]
    fn quarantine_of_a_missing_database_reports_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        let error = quarantine(&path).unwrap_err();
        assert!(matches!(&error, StoreError::Io { path: failed, .. } if failed == &path));
    }

    fn open_store(tmp: &tempfile::TempDir) -> Store {
        Store::open(&tmp.path().join("state.db")).unwrap()
    }

    /// A transaction body that does nothing.
    fn nothing(_: &Transaction<'_>) -> Result<(), StoreError> {
        Ok(())
    }

    #[test]
    fn a_transaction_commits_what_its_body_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(&tmp);
        let rows = store
            .in_transaction(|tx| {
                tx.execute("INSERT INTO kv (key, value) VALUES ('a', '1')", [])
                    .unwrap();
                Ok(7)
            })
            .unwrap();
        assert_eq!(rows, 7);
        assert_eq!(store.kv_get("a").unwrap().as_deref(), Some("1"));
        // An empty body commits too, and leaves the stored data as it was.
        store.in_transaction(nothing).unwrap();
        assert_eq!(store.kv_get("a").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn a_failing_body_rolls_back_its_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(&tmp);
        let error = store
            .in_transaction(|tx| {
                tx.execute("INSERT INTO kv (key, value) VALUES ('a', '1')", [])
                    .unwrap();
                Err::<(), StoreError>(StoreError::Cancelled)
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "operation was cancelled");
        assert_eq!(store.kv_get("a").unwrap(), None);
    }

    #[test]
    fn a_commit_that_violates_a_deferred_constraint_fails_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(&tmp);
        store
            .conn
            .execute_batch(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE child (
                     parent_id INTEGER REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED
                 );",
            )
            .unwrap();
        let error = store
            .in_transaction(|tx| {
                tx.execute("INSERT INTO child (parent_id) VALUES (7)", [])
                    .unwrap();
                Ok(())
            })
            .unwrap_err();
        assert!(
            error.to_string().contains("FOREIGN KEY constraint failed"),
            "{error}"
        );
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM child", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn a_transaction_cannot_start_inside_another_one() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = open_store(&tmp);
        store.conn.execute_batch("BEGIN").unwrap();
        let error = store.in_transaction(nothing).unwrap_err();
        assert!(
            error.to_string().contains("within a transaction"),
            "{error}"
        );
    }

    #[test]
    fn rows_are_decoded_into_tuples_and_a_wrong_column_type_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(&tmp);
        store.kv_set("one", "1").unwrap();
        store.kv_set("two", "2").unwrap();
        let rows: Vec<(String, String)> = query_rows(
            &store.conn,
            "SELECT key, value FROM kv WHERE key != ?1 ORDER BY key",
            ["one"],
        )
        .unwrap();
        assert_eq!(rows, [("two".to_owned(), "2".to_owned())]);
        let none: Option<(String,)> =
            query_optional(&store.conn, "SELECT key FROM kv WHERE key = ?1", ["three"]).unwrap();
        assert_eq!(none, None);
        let error =
            query_rows::<(i64, i64), _>(&store.conn, "SELECT key, value FROM kv", []).unwrap_err();
        assert!(error.to_string().contains("Invalid column type"), "{error}");
    }
}
