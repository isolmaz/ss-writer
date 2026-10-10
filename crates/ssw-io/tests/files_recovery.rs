use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_io::encoding::{LineEnding, TextEncoding, TextFormat};
use ssw_io::fs::{FaultFs, FileSystem, FsError, FsFailure, FsOp, RealFs};
use ssw_io::recovery::{
    BackupTimer, MANIFEST_NAME, RecoveryDoc, RecoveryEntry, RecoveryError, RecoveryKind,
    RecoveryStore, UNTITLED_BACKUP_INTERVAL, now_millis,
};
use ssw_io::save::SaveError;

fn store_in(dir: &Path) -> RecoveryStore {
    RecoveryStore::new(Arc::new(RealFs), dir.join("recovery"))
}

fn faulty_store_in(dir: &Path) -> (RecoveryStore, Arc<FaultFs>) {
    let fault = Arc::new(FaultFs::new());
    (
        RecoveryStore::new(fault.clone(), dir.join("recovery")),
        fault,
    )
}

fn untitled<'a>(id: &'a str, text: &'a str) -> RecoveryDoc<'a> {
    RecoveryDoc {
        id,
        kind: RecoveryKind::Untitled,
        original_path: None,
        display_name: "Untitled",
        text,
        format: TextFormat::default(),
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = RealFs
        .list_dir(dir)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    names
}

fn ids(store: &RecoveryStore) -> Vec<String> {
    let mut ids: Vec<String> = store
        .list()
        .unwrap()
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    ids.sort();
    ids
}

#[test]
fn the_timer_asks_for_a_backup_every_thirty_seconds() {
    assert_eq!(UNTITLED_BACKUP_INTERVAL, Duration::from_secs(30));
    let start = Instant::now();
    let mut timer = BackupTimer::new();
    assert!(timer.due(start), "the first tick is due at once");
    assert!(!timer.due(start));
    assert!(!timer.due(start + Duration::from_secs(29)));
    assert!(
        timer.due(start + Duration::from_secs(30)),
        "the interval restarts here"
    );
    assert!(!timer.due(start + Duration::from_secs(59)));
    assert!(timer.due(start + Duration::from_secs(61)));
    // A clock that appears to run backwards never panics and never fires.
    assert!(!timer.due(start));
}

#[test]
fn the_clock_gives_unix_milliseconds() {
    let first = now_millis();
    let second = now_millis();
    assert!(first > 1_700_000_000_000, "{first}");
    assert!(second >= first);
}

#[test]
fn a_backup_is_listed_and_restored_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    assert_eq!(store.dir(), dir.path().join("recovery"));
    assert!(store.list().unwrap().is_empty());

    let text = "# Taslak\n\nİstanbul'da şiir 😀\n\n- bir\n- iki\n";
    store
        .backup(&untitled("tab-1", text), 1_700_000_000_123)
        .unwrap();

    assert_eq!(names(store.dir()), ["manifest.json", "tab-1.txt"]);
    assert_eq!(
        std::fs::read(store.dir().join("tab-1.txt")).unwrap(),
        text.as_bytes()
    );
    let listed = store.list().unwrap();
    assert_eq!(
        listed,
        [RecoveryEntry {
            id: "tab-1".into(),
            kind: RecoveryKind::Untitled,
            original_path: None,
            display_name: "Untitled".into(),
            saved_at_ms: 1_700_000_000_123,
            format: TextFormat::default(),
            data_file: "tab-1.txt".into(),
        }]
    );
    let restored = store.restore("tab-1").unwrap();
    assert_eq!(restored.text, text);
    assert_eq!(restored.entry, listed[0]);
    // Restoring reads only; the entry stays until it is discarded.
    assert_eq!(ids(&store), ["tab-1"]);
}

#[test]
fn the_manifest_is_plain_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.backup(&untitled("one", "1"), 10).unwrap();
    let manifest = std::fs::read(store.dir().join(MANIFEST_NAME)).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    assert_eq!(json["version"], 1);
    let entry = &json["entries"][0];
    assert_eq!(entry["id"], "one");
    assert_eq!(entry["kind"], "Untitled");
    assert_eq!(entry["display_name"], "Untitled");
    assert_eq!(entry["saved_at_ms"], 10);
    assert_eq!(entry["data_file"], "one.txt");
    assert_eq!(entry["format"]["encoding"], "UTF-8");
    assert_eq!(json["entries"].as_array().unwrap().len(), 1);
}

#[test]
fn dirty_documents_remember_their_file_and_format_without_touching_it() {
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("notes.md");
    std::fs::write(&original, b"on disk").unwrap();
    let format = TextFormat {
        encoding: TextEncoding::from_label("windows-1254").unwrap(),
        bom: false,
        line_ending: LineEnding::Crlf,
        had_mixed_endings: true,
    };
    let store = store_in(dir.path());
    store
        .backup(
            &RecoveryDoc {
                id: "doc_1",
                kind: RecoveryKind::Dirty,
                original_path: Some(&original),
                display_name: "notes.md",
                text: "unsaved ğüşiöç",
                format,
            },
            42,
        )
        .unwrap();

    // A later session finds it with a fresh store.
    let next_launch = store_in(dir.path());
    let restored = next_launch.restore("doc_1").unwrap();
    assert_eq!(restored.text, "unsaved ğüşiöç");
    assert_eq!(restored.entry.kind, RecoveryKind::Dirty);
    assert_eq!(
        restored.entry.original_path.as_deref(),
        Some(original.as_path())
    );
    assert_eq!(restored.entry.format, format);
    assert_eq!(restored.entry.display_name, "notes.md");
    assert_eq!(std::fs::read(&original).unwrap(), b"on disk");
}

#[test]
fn backing_up_the_same_id_again_replaces_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.backup(&untitled("a", "first"), 1).unwrap();
    store.backup(&untitled("b", "other"), 2).unwrap();
    store.backup(&untitled("a", "second, longer"), 3).unwrap();
    assert_eq!(ids(&store), ["a", "b"]);
    let a = store.restore("a").unwrap();
    assert_eq!(a.text, "second, longer");
    assert_eq!(a.entry.saved_at_ms, 3);
    assert_eq!(store.restore("b").unwrap().text, "other");
    assert_eq!(
        names(store.dir()),
        ["a.txt", "b.txt", "manifest.json"],
        "no temp files remain"
    );
}

#[test]
fn only_simple_ids_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    let longest = "x".repeat(64);
    for id in [longest.as_str(), "A-b_9", "tab", "0"] {
        store.backup(&untitled(id, "ok"), 1).unwrap();
    }
    let too_long = "x".repeat(65);
    for id in [
        "",
        too_long.as_str(),
        "a b",
        "a.b",
        "a/b",
        "..\\evil",
        "é",
        "tab\n",
        "C:",
    ] {
        match store.backup(&untitled(id, "bad"), 1) {
            Err(RecoveryError::InvalidId(rejected)) => assert_eq!(rejected, id),
            other => panic!("{id:?} should be rejected, got {other:?}"),
        }
    }
    assert_eq!(ids(&store).len(), 4);
    assert_eq!(
        names(store.dir()).len(),
        5,
        "four data files and the manifest"
    );
    assert_eq!(
        RecoveryError::InvalidId("a b".into()).to_string(),
        "invalid recovery id \"a b\""
    );
}

#[test]
fn restoring_and_discarding_unknown_ids_fails() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.backup(&untitled("known", "text"), 1).unwrap();
    assert!(
        matches!(store.restore("unknown"), Err(RecoveryError::NotFound(id)) if id == "unknown")
    );
    assert!(
        matches!(store.discard("unknown"), Err(RecoveryError::NotFound(id)) if id == "unknown")
    );
    assert_eq!(ids(&store), ["known"]);
}

#[test]
fn a_missing_or_garbled_data_file_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.backup(&untitled("garbled", "fine"), 1).unwrap();
    store.backup(&untitled("gone", "fine"), 1).unwrap();
    std::fs::write(store.dir().join("garbled.txt"), [0xFF, 0xFE, 0xFD]).unwrap();
    std::fs::remove_file(store.dir().join("gone.txt")).unwrap();
    assert!(matches!(
        store.restore("garbled"),
        Err(RecoveryError::InvalidText(id)) if id == "garbled"
    ));
    assert!(matches!(
        store.restore("gone"),
        Err(RecoveryError::Fs(FsError::NotFound(_)))
    ));
    // Both can still be discarded; a data file that is already gone is not an error.
    store.discard("garbled").unwrap();
    store.discard("gone").unwrap();
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn discarding_removes_the_entry_and_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store.backup(&untitled("a", "A"), 1).unwrap();
    store.backup(&untitled("b", "B"), 2).unwrap();
    store.discard("a").unwrap();
    assert_eq!(ids(&store), ["b"]);
    assert_eq!(names(store.dir()), ["b.txt", "manifest.json"]);
    assert!(matches!(
        store.restore("a"),
        Err(RecoveryError::NotFound(_))
    ));
}

#[test]
fn discard_all_empties_the_recovery_folder() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
        store.backup(&untitled(id, id), index as u64).unwrap();
    }
    std::fs::remove_file(store.dir().join("b.txt")).unwrap();
    store.discard_all().unwrap();
    assert!(store.list().unwrap().is_empty());
    assert_eq!(names(store.dir()), ["manifest.json"]);
    // Nothing left to discard is fine too, even without a folder at all.
    store.discard_all().unwrap();
    let fresh = tempfile::tempdir().unwrap();
    let empty = store_in(fresh.path());
    empty.discard_all().unwrap();
    assert_eq!(names(empty.dir()), ["manifest.json"]);
}

#[test]
fn an_emergency_save_writes_every_document_and_one_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    store
        .backup(&untitled("keep", "kept from the last tick"), 1)
        .unwrap();
    store.backup(&untitled("refresh", "old"), 1).unwrap();
    let report = store
        .emergency_save(
            &[
                untitled("fresh", "brand new"),
                untitled("refresh", "newer text"),
                untitled("../escape", "must not be written"),
                RecoveryDoc {
                    id: "dirty",
                    kind: RecoveryKind::Dirty,
                    original_path: Some(Path::new("C:\\docs\\a.md")),
                    display_name: "a.md",
                    text: "edited",
                    format: TextFormat::default(),
                },
            ],
            99,
        )
        .unwrap();
    assert_eq!(report.saved, ["fresh", "refresh", "dirty"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "../escape");
    assert!(matches!(&report.failed[0].1, RecoveryError::InvalidId(id) if id == "../escape"));

    assert_eq!(ids(&store), ["dirty", "fresh", "keep", "refresh"]);
    assert_eq!(
        store.restore("keep").unwrap().text,
        "kept from the last tick"
    );
    assert_eq!(store.restore("refresh").unwrap().text, "newer text");
    assert_eq!(store.restore("refresh").unwrap().entry.saved_at_ms, 99);
    let dirty = store.restore("dirty").unwrap();
    assert_eq!(
        dirty.entry.original_path,
        Some(PathBuf::from("C:\\docs\\a.md"))
    );
    assert_eq!(dirty.text, "edited");
    assert!(!dir.path().join("escape.txt").exists());
}

#[test]
fn an_emergency_save_replaces_a_corrupt_or_unreadable_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.dir().join(MANIFEST_NAME), "{ this is not json").unwrap();
    assert!(matches!(store.list(), Err(RecoveryError::Manifest(_))));
    assert!(matches!(
        store.backup(&untitled("x", "x"), 1),
        Err(RecoveryError::Manifest(_))
    ));
    assert!(matches!(
        store.restore("x"),
        Err(RecoveryError::Manifest(_))
    ));
    assert!(matches!(
        store.discard("x"),
        Err(RecoveryError::Manifest(_))
    ));
    assert!(matches!(
        store.discard_all(),
        Err(RecoveryError::Manifest(_))
    ));

    let report = store
        .emergency_save(&[untitled("saved", "text")], 5)
        .unwrap();
    assert_eq!(report.saved, ["saved"]);
    assert!(report.failed.is_empty());
    assert_eq!(
        ids(&store),
        ["saved"],
        "the documents survive a damaged manifest"
    );

    let (faulty, fault) = faulty_store_in(dir.path());
    let manifest = faulty.dir().join(MANIFEST_NAME);
    fault.fail_for_path(FsOp::Read, &manifest, FsFailure::AccessDenied, 1);
    assert!(matches!(
        faulty.list(),
        Err(RecoveryError::Fs(FsError::AccessDenied(_)))
    ));
    fault.fail_for_path(FsOp::Read, &manifest, FsFailure::AccessDenied, 1);
    let report = faulty
        .emergency_save(&[untitled("again", "more")], 6)
        .unwrap();
    assert_eq!(report.saved, ["again"]);
    assert_eq!(ids(&faulty), ["again"]);
}

#[test]
fn an_emergency_save_continues_after_a_failing_document() {
    let dir = tempfile::tempdir().unwrap();
    let (store, fault) = faulty_store_in(dir.path());
    fault.fail(FsOp::WriteNew, FsFailure::DiskFull, 1);
    let report = store
        .emergency_save(&[untitled("first", "1"), untitled("second", "2")], 1)
        .unwrap();
    assert_eq!(report.saved, ["second"]);
    assert_eq!(report.failed.len(), 1);
    assert_eq!(report.failed[0].0, "first");
    assert!(matches!(
        &report.failed[0].1,
        RecoveryError::Save(SaveError::DiskFull(_))
    ));
    assert_eq!(ids(&store), ["second"]);
}

#[test]
fn write_failures_are_reported_without_leaving_half_written_state() {
    let dir = tempfile::tempdir().unwrap();
    let (store, fault) = faulty_store_in(dir.path());

    fault.fail(FsOp::CreateDirAll, FsFailure::AccessDenied, 1);
    assert!(matches!(
        store.backup(&untitled("a", "A"), 1),
        Err(RecoveryError::Fs(FsError::AccessDenied(_)))
    ));
    assert!(!store.dir().exists());

    fault.fail_always(FsOp::WriteNew, FsFailure::DiskFull);
    assert!(matches!(
        store.backup(&untitled("a", "A"), 1),
        Err(RecoveryError::Save(SaveError::DiskFull(_)))
    ));
    // Even the final manifest write of an emergency save can fail.
    assert!(matches!(
        store.emergency_save(&[untitled("a", "A")], 1),
        Err(RecoveryError::Save(SaveError::DiskFull(_)))
    ));
    fault.clear();
    assert!(!store.dir().join("a.txt").exists());

    // A good entry survives a failing manifest update during discard.
    store.backup(&untitled("good", "G"), 1).unwrap();
    fault.fail_always(FsOp::WriteNew, FsFailure::DiskFull);
    assert!(matches!(
        store.discard("good"),
        Err(RecoveryError::Save(SaveError::DiskFull(_)))
    ));
    assert!(matches!(
        store.discard_all(),
        Err(RecoveryError::Save(SaveError::DiskFull(_)))
    ));
    fault.clear();
    assert_eq!(store.restore("good").unwrap().text, "G");
    assert_eq!(names(store.dir()), ["good.txt", "manifest.json"]);
}

#[test]
fn a_path_that_is_not_unicode_cannot_be_recorded_in_the_manifest() {
    use std::os::windows::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    // An unpaired surrogate is a legal Windows file name but not valid UTF-8 for JSON.
    let odd = std::ffi::OsString::from_wide(&[0x43, 0x3A, 0x5C, 0xD800]);
    let doc = RecoveryDoc {
        id: "odd",
        kind: RecoveryKind::Dirty,
        original_path: Some(Path::new(&odd)),
        display_name: "odd",
        text: "text",
        format: TextFormat::default(),
    };
    assert!(matches!(
        store.backup(&doc, 1),
        Err(RecoveryError::Manifest(_))
    ));
    assert!(matches!(
        store.emergency_save(&[doc], 1),
        Err(RecoveryError::Manifest(_))
    ));
    assert!(store.list().unwrap().is_empty());
}
