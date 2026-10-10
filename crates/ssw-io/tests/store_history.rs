//! Version history: snapshot policy, deduplication, retention, restore.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use ssw_io::history::{
    SNAPSHOT_INTERVAL_SECS, Snapshot, SnapshotInfo, SnapshotOutcome, SnapshotReason,
    should_snapshot,
};
use ssw_io::store::{Store, StoreError};
use tempfile::{TempDir, tempdir};

const DAY: i64 = 24 * 60 * 60;

fn open() -> (TempDir, Store, PathBuf) {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("state.db");
    let store = Store::open(&path).unwrap();
    (tmp, store, path)
}

fn blob_count(db: &Path) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM history_blobs", [], |row| row.get(0))
        .unwrap()
}

fn created(outcome: SnapshotOutcome) -> i64 {
    match outcome {
        SnapshotOutcome::Created(id) => id,
        SnapshotOutcome::Unchanged(id) => panic!("expected a new snapshot, got unchanged {id}"),
    }
}

#[test]
fn periodic_snapshots_wait_for_the_interval() {
    let now = 1_000_000;
    let cases = [
        (None, true),
        (Some(now - 1), false),
        (Some(now - (SNAPSHOT_INTERVAL_SECS - 1)), false),
        (Some(now - SNAPSHOT_INTERVAL_SECS), true),
        (Some(now - 3600), true),
        // The clock went backwards: never snapshot because of a negative age.
        (Some(now + 10), false),
        (Some(i64::MIN), true),
    ];
    for (last, expected) in cases {
        assert_eq!(
            should_snapshot(now, last, SnapshotReason::Periodic),
            expected,
            "last = {last:?}"
        );
    }
    assert_eq!(SNAPSHOT_INTERVAL_SECS, 300);
}

#[test]
fn explicit_events_always_snapshot() {
    for reason in [
        SnapshotReason::Close,
        SnapshotReason::BeforeExternalReload,
        SnapshotReason::BeforeSaveBack,
        SnapshotReason::BeforeRestore,
    ] {
        assert!(should_snapshot(100, None, reason));
        assert!(should_snapshot(100, Some(99), reason));
        assert!(should_snapshot(100, Some(500), reason));
    }
}

#[test]
fn identical_content_is_not_stored_twice_in_a_row() {
    let (_tmp, mut store, db) = open();
    let file = Path::new("C:\\notes\\a.md");
    let first = created(
        store
            .history_snapshot(file, "# One\n", SnapshotReason::Periodic, 100)
            .unwrap(),
    );
    // Same text, any reason, any time later: nothing new.
    for (reason, now) in [
        (SnapshotReason::Periodic, 500),
        (SnapshotReason::Close, 600),
        (SnapshotReason::BeforeSaveBack, 700),
    ] {
        assert_eq!(
            store
                .history_snapshot(file, "# One\n", reason, now)
                .unwrap(),
            SnapshotOutcome::Unchanged(first)
        );
    }
    assert_eq!(store.history_list(file).unwrap().len(), 1);

    let second = created(
        store
            .history_snapshot(file, "# Two\n", SnapshotReason::Close, 800)
            .unwrap(),
    );
    assert_ne!(first, second);

    // Going back to the first text is a new version but shares its stored blob.
    let third = created(
        store
            .history_snapshot(file, "# One\n", SnapshotReason::Periodic, 900)
            .unwrap(),
    );
    assert!(third > second);
    assert_eq!(store.history_list(file).unwrap().len(), 3);
    assert_eq!(blob_count(&db), 2);
}

#[test]
fn files_have_independent_histories_and_paths_compare_case_insensitively() {
    let (_tmp, mut store, _) = open();
    let a = Path::new("C:\\Notes\\A.md");
    let b = Path::new("C:\\Notes\\B.md");
    store
        .history_snapshot(a, "same", SnapshotReason::Close, 1)
        .unwrap();
    // The same text in another file is a new snapshot there.
    assert!(matches!(
        store
            .history_snapshot(b, "same", SnapshotReason::Close, 2)
            .unwrap(),
        SnapshotOutcome::Created(_)
    ));
    // Another spelling of the first path finds its newest snapshot.
    assert!(matches!(
        store
            .history_snapshot(Path::new("c:/notes/a.MD"), "same", SnapshotReason::Close, 3)
            .unwrap(),
        SnapshotOutcome::Unchanged(_)
    ));
    assert_eq!(
        store
            .history_list(Path::new("c:/NOTES/a.md"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(store.history_list(b).unwrap().len(), 1);
    assert_eq!(
        store
            .history_last_snapshot_time(Path::new("C:\\notes\\b.MD"))
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        store
            .history_last_snapshot_time(Path::new("C:\\notes\\none.md"))
            .unwrap(),
        None
    );
}

#[test]
fn list_and_load_return_what_was_stored() {
    let (_tmp, mut store, _) = open();
    let file = Path::new("C:\\notes\\a.md");
    let reasons = [
        SnapshotReason::Periodic,
        SnapshotReason::Close,
        SnapshotReason::BeforeExternalReload,
        SnapshotReason::BeforeSaveBack,
        SnapshotReason::BeforeRestore,
    ];
    let texts = ["héllo", "wörld ☃", "", "line\r\nbreaks\n", "𝄞"];
    let mut ids = Vec::new();
    for (i, (reason, text)) in reasons.into_iter().zip(texts).enumerate() {
        ids.push(created(
            store
                .history_snapshot(file, text, reason, 1000 + i as i64)
                .unwrap(),
        ));
    }

    let listed = store.history_list(file).unwrap();
    let expected: Vec<SnapshotInfo> = (0..5)
        .rev()
        .map(|i| SnapshotInfo {
            id: ids[i],
            created_at: 1000 + i as i64,
            reason: reasons[i],
            size: texts[i].len(),
        })
        .collect();
    assert_eq!(listed, expected, "newest first, sizes in UTF-8 bytes");

    for (i, id) in ids.iter().enumerate() {
        assert_eq!(
            store.history_load(*id).unwrap(),
            Some(Snapshot {
                info: expected[4 - i],
                content: texts[i].to_owned()
            })
        );
    }
    assert_eq!(store.history_load(9999).unwrap(), None);
    assert_eq!(store.history_last_snapshot_time(file).unwrap(), Some(1004));
    assert!(
        store
            .history_list(Path::new("C:\\other.md"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn restoring_snapshots_the_current_text_first_so_it_can_be_undone() {
    let (_tmp, mut store, _) = open();
    let file = Path::new("C:\\notes\\a.md");
    let old = created(
        store
            .history_snapshot(file, "old text", SnapshotReason::Close, 10)
            .unwrap(),
    );
    store
        .history_snapshot(file, "newer text", SnapshotReason::Close, 20)
        .unwrap();

    let restored = store
        .history_restore(file, old, "unsaved current text", 30)
        .unwrap();
    assert_eq!(restored.as_deref(), Some("old text"));
    let list = store.history_list(file).unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].reason, SnapshotReason::BeforeRestore);
    assert_eq!(list[0].created_at, 30);
    assert_eq!(
        store.history_load(list[0].id).unwrap().unwrap().content,
        "unsaved current text"
    );

    // When the current text is already the newest snapshot nothing is added.
    let again = store
        .history_restore(file, old, "unsaved current text", 40)
        .unwrap();
    assert_eq!(again.as_deref(), Some("old text"));
    assert_eq!(store.history_list(file).unwrap().len(), 3);
}

#[test]
fn restoring_a_missing_snapshot_stores_nothing() {
    let (_tmp, mut store, _) = open();
    let file = Path::new("C:\\notes\\a.md");
    assert_eq!(store.history_restore(file, 77, "current", 5).unwrap(), None);
    assert!(store.history_list(file).unwrap().is_empty());
}

#[test]
fn purge_removes_snapshots_older_than_the_retention_and_unreferenced_blobs() {
    let (_tmp, mut store, db) = open();
    let file = Path::new("C:\\notes\\a.md");
    let now = 100 * DAY;
    // Ages: 50 days (unique text), 31 days (text shared with the 1-day-old snapshot),
    // exactly 30 days, 1 day.
    store
        .history_snapshot(file, "ancient", SnapshotReason::Close, now - 50 * DAY)
        .unwrap();
    store
        .history_snapshot(file, "shared", SnapshotReason::Close, now - 31 * DAY)
        .unwrap();
    store
        .history_snapshot(file, "boundary", SnapshotReason::Close, now - 30 * DAY)
        .unwrap();
    store
        .history_snapshot(file, "shared", SnapshotReason::Close, now - DAY)
        .unwrap();
    assert_eq!(blob_count(&db), 3);

    // Retention 0 keeps everything.
    assert_eq!(store.history_purge(now, 0).unwrap(), 0);
    assert_eq!(store.history_list(file).unwrap().len(), 4);

    assert_eq!(store.history_purge(now, 30).unwrap(), 2);
    let kept: Vec<i64> = store
        .history_list(file)
        .unwrap()
        .iter()
        .map(|info| info.created_at)
        .collect();
    assert_eq!(kept, vec![now - DAY, now - 30 * DAY]);
    // "ancient" is gone; "shared" survives through the newer snapshot.
    assert_eq!(blob_count(&db), 2);
    let newest = store.history_list(file).unwrap()[0].id;
    assert_eq!(
        store.history_load(newest).unwrap().unwrap().content,
        "shared"
    );

    // Purging again deletes nothing.
    assert_eq!(store.history_purge(now, 30).unwrap(), 0);
    // A shorter retention takes the boundary snapshot too.
    assert_eq!(store.history_purge(now, 7).unwrap(), 1);
    assert_eq!(store.history_list(file).unwrap().len(), 1);
}

#[test]
fn moving_a_file_carries_its_history_along() {
    let (_tmp, mut store, _) = open();
    let from = Path::new("C:\\notes\\Draft.md");
    let to = Path::new("C:\\notes\\archive\\Final.md");
    store
        .history_snapshot(from, "v1", SnapshotReason::Close, 1)
        .unwrap();
    store
        .history_snapshot(from, "v2", SnapshotReason::Close, 2)
        .unwrap();
    store
        .history_move(Path::new("c:/NOTES/draft.md"), to)
        .unwrap();
    assert!(store.history_list(from).unwrap().is_empty());
    let moved = store.history_list(to).unwrap();
    assert_eq!(moved.len(), 2);
    // The next snapshot continues the moved history (and dedupes against it).
    assert_eq!(
        store
            .history_snapshot(to, "v2", SnapshotReason::Close, 3)
            .unwrap(),
        SnapshotOutcome::Unchanged(moved[0].id)
    );
}

#[test]
fn unrepresentable_paths_are_rejected() {
    let (_tmp, mut store, _) = open();
    let bad = PathBuf::from(OsString::from_wide(&[0x41, 0xD800]));
    let good = Path::new("C:\\ok.md");
    let rejected = |error: StoreError| matches!(error, StoreError::NonUnicodePath(_));
    assert!(rejected(
        store
            .history_snapshot(&bad, "x", SnapshotReason::Close, 1)
            .unwrap_err()
    ));
    assert!(rejected(
        store.history_last_snapshot_time(&bad).unwrap_err()
    ));
    assert!(rejected(store.history_list(&bad).unwrap_err()));
    assert!(rejected(store.history_move(&bad, good).unwrap_err()));
    assert!(rejected(store.history_move(good, &bad).unwrap_err()));
}

#[test]
fn damaged_rows_are_reported_as_invalid_data() {
    let (_tmp, store, db) = open();
    let raw = Connection::open(&db).unwrap();
    raw.execute_batch(
        "INSERT INTO history_blobs (hash, content) VALUES (zeroblob(32), x'FFFE');
         INSERT INTO history_blobs (hash, content) VALUES (zeroblob(31), x'41');
         INSERT INTO history_snapshots (id, key, path, created_at, reason, hash)
         VALUES (1, 'C:\\BAD.MD', 'C:\\bad.md', 1, 0, zeroblob(32)),
                (2, 'C:\\WORSE.MD', 'C:\\worse.md', 1, 99, zeroblob(31));",
    )
    .unwrap();

    let not_utf8 = store.history_load(1).unwrap_err();
    assert!(
        matches!(not_utf8, StoreError::InvalidData(_)),
        "{not_utf8:?}"
    );
    assert!(not_utf8.to_string().contains("UTF-8"));

    let bad_reason = store.history_load(2).unwrap_err();
    assert!(
        matches!(bad_reason, StoreError::InvalidData(_)),
        "{bad_reason:?}"
    );
    let listed = store.history_list(Path::new("C:\\worse.md")).unwrap_err();
    assert!(matches!(listed, StoreError::InvalidData(_)), "{listed:?}");
}

#[test]
fn history_queries_report_sqlite_failures_when_the_tables_are_gone() {
    let (_tmp, store, db) = open();
    Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE history_snapshots; DROP TABLE history_blobs;")
        .unwrap();
    let path = Path::new("C:\\a.md");
    let errors = [
        store.history_last_snapshot_time(path).unwrap_err(),
        store.history_list(path).unwrap_err(),
        store.history_move(path, Path::new("C:\\b.md")).unwrap_err(),
    ];
    for error in errors {
        assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
    }
}
