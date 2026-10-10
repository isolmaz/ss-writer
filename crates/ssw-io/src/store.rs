//! The SQLite state database (`state.db`): connection setup, versioned migrations, corruption
//! recovery and a small key/value table.
//!
//! The feature areas live in their own modules as further `impl Store` blocks: [`crate::session`]
//! (windows, tabs, recent items, per-file view state, untitled buffers), [`crate::history`]
//! (snapshots), [`crate::index`] (workspace index) and [`crate::remote_images`] (cache
//! metadata). Every timestamp is a Unix time supplied by the caller so tests inject the clock.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OptionalExtension};
use thiserror::Error;

/// Unix time in seconds.
pub type UnixSeconds = i64;

/// How long a connection waits for a lock held by another connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
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
                let aside = quarantine(path)?;
                Ok(Self {
                    conn: open_connection(path)?,
                    path: path.to_path_buf(),
                    quarantined: Some(aside),
                })
            }
            Err(other) => Err(other),
        }
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
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
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

/// Connect, configure and migrate.
fn open_connection(path: &Path) -> Result<Connection, StoreError> {
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if !(0..=latest_schema_version()).contains(&version) {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            supported: latest_schema_version(),
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
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
}
