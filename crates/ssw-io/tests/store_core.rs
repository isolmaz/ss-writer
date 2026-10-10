//! App data locations and the SQLite state database: migrations, corruption recovery, key/value.

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use ssw_io::paths::{
    APP_DIR_NAME, AppPaths, Environment, PORTABLE_DATA_DIR, PORTABLE_MARKER, PathsError,
};
use ssw_io::store::{MIGRATIONS, Store, StoreError, latest_schema_version, path_key};
use tempfile::tempdir;

fn env(root: &Path) -> Environment {
    Environment {
        local_app_data: Some(root.join("local")),
        app_data: Some(root.join("roaming")),
        exe_dir: root.join("app"),
    }
}

#[test]
fn normal_mode_keeps_state_local_and_settings_roaming() {
    let tmp = tempdir().unwrap();
    let paths = AppPaths::resolve(&env(tmp.path())).unwrap();
    let local = tmp.path().join("local").join(APP_DIR_NAME);
    let roaming = tmp.path().join("roaming").join(APP_DIR_NAME);
    assert!(!paths.is_portable());
    assert_eq!(paths.local_dir(), local.as_path());
    assert_eq!(paths.roaming_dir(), roaming.as_path());
    assert_eq!(paths.state_db(), local.join("state.db"));
    assert_eq!(paths.settings_file(), roaming.join("settings.json"));
    assert_eq!(paths.recovery_dir(), local.join("recovery"));
    assert_eq!(paths.crashes_dir(), local.join("crashes"));
    assert_eq!(paths.logs_dir(), local.join("logs"));
    assert_eq!(paths.cache_dir(), local.join("cache"));
    assert_eq!(paths.image_cache_dir(), local.join("cache").join("images"));
}

#[test]
fn portable_marker_moves_everything_next_to_the_executable() {
    let tmp = tempdir().unwrap();
    let mut environment = env(tmp.path());
    // Portable mode does not need the user profile variables at all.
    environment.local_app_data = None;
    environment.app_data = None;
    std::fs::create_dir_all(&environment.exe_dir).unwrap();
    std::fs::write(environment.exe_dir.join(PORTABLE_MARKER), b"").unwrap();

    let paths = AppPaths::resolve(&environment).unwrap();
    let data = environment.exe_dir.join(PORTABLE_DATA_DIR);
    assert!(paths.is_portable());
    assert_eq!(paths.local_dir(), data.as_path());
    assert_eq!(paths.roaming_dir(), data.as_path());
    assert_eq!(paths.state_db(), data.join("state.db"));
    assert_eq!(paths.settings_file(), data.join("settings.json"));
}

#[test]
fn a_folder_named_portable_does_not_enable_portable_mode() {
    let tmp = tempdir().unwrap();
    let environment = env(tmp.path());
    std::fs::create_dir_all(environment.exe_dir.join(PORTABLE_MARKER)).unwrap();
    assert!(!AppPaths::resolve(&environment).unwrap().is_portable());
}

#[test]
fn missing_profile_variables_are_reported_by_name() {
    let tmp = tempdir().unwrap();
    let mut no_local = env(tmp.path());
    no_local.local_app_data = None;
    let error = AppPaths::resolve(&no_local).unwrap_err();
    assert!(matches!(error, PathsError::MissingEnv("LOCALAPPDATA")));
    assert!(error.to_string().contains("LOCALAPPDATA"));

    let mut no_roaming = env(tmp.path());
    no_roaming.app_data = None;
    let error = AppPaths::resolve(&no_roaming).unwrap_err();
    assert!(matches!(error, PathsError::MissingEnv("APPDATA")));
}

#[test]
fn environment_is_read_through_the_injected_lookup() {
    let get = |name: &str| match name {
        "LOCALAPPDATA" => Some(OsString::from("C:\\L")),
        "APPDATA" => Some(OsString::from("C:\\R")),
        _ => None,
    };
    let environment =
        Environment::from_vars(get, Path::new("C:\\Apps\\SSW\\SSWriter.exe")).unwrap();
    assert_eq!(environment.local_app_data, Some(PathBuf::from("C:\\L")));
    assert_eq!(environment.app_data, Some(PathBuf::from("C:\\R")));
    assert_eq!(environment.exe_dir, PathBuf::from("C:\\Apps\\SSW"));

    let bare = Environment::from_vars(|_| None, Path::new("C:\\Apps\\SSWriter.exe")).unwrap();
    assert_eq!(bare.local_app_data, None);
    assert_eq!(bare.app_data, None);

    // The same lookup (a non-capturing closure is `Copy`), so one instantiation of `from_vars`
    // sees both outcomes.
    let error = Environment::from_vars(get, Path::new("C:\\")).unwrap_err();
    assert!(matches!(error, PathsError::NoParent(_)));
    assert!(error.to_string().contains("C:\\"));
}

#[test]
fn process_environment_points_at_the_running_executable() {
    let exe = std::env::current_exe().unwrap();
    let environment = Environment::from_process().unwrap();
    assert_eq!(Some(environment.exe_dir.as_path()), exe.parent());
    // The test executable has no `portable` marker next to it.
    let paths = AppPaths::from_process().unwrap();
    assert!(!paths.is_portable());
    assert!(paths.local_dir().ends_with(APP_DIR_NAME));
}

#[test]
fn ensure_dirs_creates_the_whole_layout() {
    let tmp = tempdir().unwrap();
    let paths = AppPaths::resolve(&env(tmp.path())).unwrap();
    paths.ensure_dirs().unwrap();
    for dir in [
        paths.local_dir().to_path_buf(),
        paths.roaming_dir().to_path_buf(),
        paths.recovery_dir(),
        paths.crashes_dir(),
        paths.logs_dir(),
        paths.image_cache_dir(),
    ] {
        assert!(dir.is_dir(), "{} was not created", dir.display());
    }
    // Idempotent.
    paths.ensure_dirs().unwrap();
}

#[test]
fn ensure_dirs_names_the_folder_it_cannot_create() {
    let tmp = tempdir().unwrap();
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"a file, not a folder").unwrap();
    let mut environment = env(tmp.path());
    environment.local_app_data = Some(blocker.clone());
    let error = AppPaths::resolve(&environment)
        .unwrap()
        .ensure_dirs()
        .unwrap_err();
    assert!(matches!(&error, PathsError::CreateDir { path, .. } if path.starts_with(&blocker)));
    assert!(error.to_string().contains("blocker"));
}

#[test]
fn current_exe_failure_is_described() {
    let error = PathsError::CurrentExe(io::Error::other("no exe"));
    assert!(error.to_string().contains("no exe"));
}

#[test]
fn fresh_database_is_at_the_latest_version_with_wal_journal() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("deep").join("state.db");
    let store = Store::open(&path).unwrap();
    assert_eq!(store.path(), path.as_path());
    assert!(store.quarantined().is_none());
    assert_eq!(store.schema_version().unwrap(), latest_schema_version());
    assert_eq!(latest_schema_version(), MIGRATIONS.len() as i64);

    // The journal mode is stored in the database file, so a second connection sees it.
    let other = Connection::open(&path).unwrap();
    let mode: String = other
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn every_older_schema_is_migrated_to_the_latest() {
    for old in 0..MIGRATIONS.len() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            for sql in &MIGRATIONS[..old] {
                conn.execute_batch(sql).unwrap();
            }
            conn.pragma_update(None, "user_version", old as i64)
                .unwrap();
            if old >= 1 {
                conn.execute("INSERT INTO kv (key, value) VALUES ('k', 'kept')", [])
                    .unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.schema_version().unwrap(),
            latest_schema_version(),
            "upgrading from v{old}"
        );
        assert!(store.quarantined().is_none());
        if old >= 1 {
            assert_eq!(store.kv_get("k").unwrap().as_deref(), Some("kept"));
        }
        drop(store);

        let conn = Connection::open(&path).unwrap();
        for table in [
            "session_windows",
            "session_tabs",
            "recent_items",
            "file_view_state",
            "untitled_buffers",
            "kv",
            "history_blobs",
            "history_snapshots",
            "index_roots",
            "index_files",
            "index_headings",
            "index_links",
            "image_cache",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "table {table} after upgrading from v{old}");
        }
    }
}

#[test]
fn reopening_keeps_data_and_does_not_migrate_again() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    Store::open(&path).unwrap().kv_set("answer", "42").unwrap();
    let store = Store::open(&path).unwrap();
    assert_eq!(store.kv_get("answer").unwrap().as_deref(), Some("42"));
    assert_eq!(store.schema_version().unwrap(), latest_schema_version());
}

#[test]
fn database_from_a_newer_version_is_rejected_untouched() {
    for version in [latest_schema_version() + 1, -3] {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("state.db");
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", version)
            .unwrap();
        let error = Store::open(&path).unwrap_err();
        assert!(
            matches!(
                error,
                StoreError::UnsupportedSchema { found, supported }
                    if found == version && supported == latest_schema_version()
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains(&version.to_string()));
        // Nothing was moved aside.
        assert!(!tmp.path().join("state.db.corrupt").exists());
    }
}

#[test]
fn garbage_file_is_moved_aside_and_replaced_by_a_fresh_database() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    std::fs::write(&path, vec![b'x'; 4096]).unwrap();

    let store = Store::open(&path).unwrap();
    let aside = tmp.path().join("state.db.corrupt");
    assert_eq!(store.quarantined(), Some(aside.as_path()));
    assert_eq!(std::fs::read(&aside).unwrap(), vec![b'x'; 4096]);
    store.kv_set("a", "b").unwrap();
    assert_eq!(store.schema_version().unwrap(), latest_schema_version());
    drop(store);

    // A second corrupt database gets the next free name instead of overwriting the first.
    std::fs::write(&path, vec![b'y'; 2048]).unwrap();
    let store = Store::open(&path).unwrap();
    let second = tmp.path().join("state.db.corrupt1");
    assert_eq!(store.quarantined(), Some(second.as_path()));
    assert_eq!(std::fs::read(&second).unwrap(), vec![b'y'; 2048]);
    assert_eq!(std::fs::read(&aside).unwrap(), vec![b'x'; 4096]);
    assert_eq!(store.kv_get("a").unwrap(), None);
}

#[test]
fn corrupt_database_that_cannot_be_moved_is_an_io_error() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    std::fs::write(&path, vec![b'x'; 4096]).unwrap();
    // A handle that shares read/write but not delete access makes the rename fail while
    // SQLite can still open the file.
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&path)
        .unwrap();
    let error = Store::open(&path).unwrap_err();
    assert!(
        matches!(&error, StoreError::Io { path: failed, .. } if failed == &path),
        "{error:?}"
    );
    assert!(error.to_string().contains("state.db"));
    drop(held);
}

#[test]
fn folder_that_cannot_be_created_is_an_io_error() {
    let tmp = tempdir().unwrap();
    let blocker = tmp.path().join("blocker");
    std::fs::write(&blocker, b"file").unwrap();
    let error = Store::open(&blocker.join("state.db")).unwrap_err();
    assert!(
        matches!(&error, StoreError::Io { path, .. } if path == &blocker),
        "{error:?}"
    );
}

#[test]
fn database_path_that_is_a_folder_is_a_database_error_not_corruption() {
    let tmp = tempdir().unwrap();
    let error = Store::open(tmp.path()).unwrap_err();
    assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
    assert!(error.to_string().starts_with("database error"));
}

#[test]
fn key_value_pairs_are_stored_replaced_and_independent() {
    let tmp = tempdir().unwrap();
    let store = Store::open(&tmp.path().join("state.db")).unwrap();
    assert_eq!(store.kv_get("missing").unwrap(), None);
    store.kv_set("one", "1").unwrap();
    store.kv_set("two", "2").unwrap();
    store.kv_set("one", "uno").unwrap();
    assert_eq!(store.kv_get("one").unwrap().as_deref(), Some("uno"));
    assert_eq!(store.kv_get("two").unwrap().as_deref(), Some("2"));
    store.kv_set("empty", "").unwrap();
    assert_eq!(store.kv_get("empty").unwrap().as_deref(), Some(""));
}

#[test]
fn path_keys_ignore_case_and_slash_direction() {
    let a = path_key(Path::new("C:/Users/Ünal/Notes/Doc.md")).unwrap();
    let b = path_key(Path::new("c:\\users\\ünal\\notes\\DOC.MD")).unwrap();
    assert_eq!(a, b);
    assert_ne!(
        a,
        path_key(Path::new("C:/Users/Ünal/Notes/Other.md")).unwrap()
    );
}

#[test]
fn paths_with_unpaired_surrogates_are_rejected() {
    let bad = PathBuf::from(OsString::from_wide(&[0x61, 0xD800, 0x62]));
    let error = path_key(&bad).unwrap_err();
    assert!(matches!(error, StoreError::NonUnicodePath(_)));
    assert!(error.to_string().contains("not valid Unicode"));
}

#[test]
fn store_errors_describe_themselves() {
    let json = serde_json::from_str::<Vec<u8>>("nope").unwrap_err();
    let cases = [
        (
            StoreError::from(rusqlite::Error::QueryReturnedNoRows),
            "database error",
        ),
        (
            StoreError::Io {
                path: PathBuf::from("C:\\x\\state.db"),
                source: io::Error::other("disk on fire"),
            },
            "disk on fire",
        ),
        (StoreError::InvalidData("bad row"), "bad row"),
        (StoreError::from(json), "stored JSON is invalid"),
        (StoreError::Cancelled, "cancelled"),
    ];
    for (error, expected) in cases {
        assert!(
            error.to_string().contains(expected),
            "{error} should mention {expected}"
        );
    }
}

#[test]
fn a_database_below_a_regular_file_cannot_be_created() {
    let tmp = tempdir().unwrap();
    let plain = tmp.path().join("plain");
    std::fs::write(&plain, b"x").unwrap();
    let parent = plain.join("sub");
    let error = Store::open(&parent.join("state.db")).unwrap_err();
    assert!(
        matches!(&error, StoreError::Io { path, .. } if *path == parent),
        "{error:?}"
    );
}

#[test]
fn key_value_writes_report_a_missing_table() {
    let tmp = tempdir().unwrap();
    let db = tmp.path().join("state.db");
    let store = Store::open(&db).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE kv")
        .unwrap();
    let error = store.kv_set("k", "v").unwrap_err();
    assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
}
