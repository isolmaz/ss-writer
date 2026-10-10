use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ssw_io::fs::{Attributes, FaultFs, FileSystem, FsError, FsFailure, FsOp, RealFs};
use ssw_io::save::{
    SHARING_BACKOFF, SaveError, SaveStamp, Sleeper, ThreadSleeper, WriteAccess, allow_editing,
    content_hash, save_atomic, write_access,
};

#[derive(Default)]
struct RecordingSleeper(Mutex<Vec<Duration>>);

impl Sleeper for RecordingSleeper {
    fn sleep(&self, duration: Duration) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(duration);
    }
}

impl RecordingSleeper {
    fn millis(&self) -> Vec<u128> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(Duration::as_millis)
            .collect()
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = RealFs
        .list_dir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    names
}

#[test]
fn saves_a_new_file_and_returns_its_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.md");
    let stamp = save_atomic(&RealFs, &ThreadSleeper, &path, b"# Title\n").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"# Title\n");
    let meta = RealFs.metadata(&path).unwrap();
    assert_eq!((stamp.size, stamp.mtime), (8, meta.modified));
    assert_eq!(stamp.hash, content_hash(b"# Title\n"));
    assert!(stamp.matches_meta(&meta));
    assert!(stamp.matches_content(b"# Title\n"));
    assert!(!stamp.matches_content(b"# Title!\n"));
    assert!(!stamp.matches_content(b"# Title\n\n"));
    assert_eq!(names(dir.path()), ["new.md"], "no temp file left behind");
}

#[test]
fn replaces_an_existing_file_and_keeps_its_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, b"old").unwrap();
    RealFs
        .set_attributes(&path, Attributes(Attributes::HIDDEN))
        .unwrap();
    let before = RealFs.metadata(&path).unwrap();
    let stamp = save_atomic(&RealFs, &ThreadSleeper, &path, b"brand new content").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"brand new content");
    let after = RealFs.metadata(&path).unwrap();
    assert!(after.attrs.is_hidden());
    assert_eq!(after.created, before.created);
    assert_eq!(stamp.size, 17);
    assert_eq!(names(dir.path()), ["doc.md"]);
}

#[test]
fn stamp_from_read_and_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    std::fs::write(&path, b"abc").unwrap();
    let meta = RealFs.metadata(&path).unwrap();
    let stamp = SaveStamp::from_read(&meta, b"abc");
    assert!(stamp.matches_meta(&meta));
    assert!(stamp.matches_content(b"abc"));
    let mut longer = meta;
    longer.size = 4;
    assert!(!stamp.matches_meta(&longer));
    let mut newer = meta;
    newer.modified.0 += 1;
    assert!(!stamp.matches_meta(&newer));
}

#[test]
fn read_only_files_are_refused_until_editing_is_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.md");
    std::fs::write(&path, b"locked").unwrap();
    RealFs
        .set_attributes(&path, Attributes(Attributes::READONLY))
        .unwrap();
    assert_eq!(
        write_access(&RealFs, &path).unwrap(),
        WriteAccess::ReadOnlyAttribute
    );
    let error = save_atomic(&RealFs, &ThreadSleeper, &path, b"x").unwrap_err();
    assert_eq!(error, SaveError::ReadOnly(path.clone()));
    assert!(error.to_string().contains("read-only"));
    assert_eq!(std::fs::read(&path).unwrap(), b"locked");

    allow_editing(&RealFs, &path).unwrap();
    assert_eq!(write_access(&RealFs, &path).unwrap(), WriteAccess::Writable);
    save_atomic(&RealFs, &ThreadSleeper, &path, b"unlocked").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"unlocked");
    // Nothing to clear on a writable file.
    allow_editing(&RealFs, &path).unwrap();
    assert!(matches!(
        write_access(&RealFs, &dir.path().join("missing")),
        Err(FsError::NotFound(_))
    ));
    assert!(matches!(
        allow_editing(&RealFs, &dir.path().join("missing")),
        Err(FsError::NotFound(_))
    ));
}

#[test]
fn failing_access_checks_are_reported_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("checked.md");
    std::fs::write(&path, b"x").unwrap();
    let code = FsFailure::Code(1234);
    let os = FsError::Os {
        code: 1234,
        path: path.clone(),
    };

    // The write test of a writable-looking file.
    let fs = FaultFs::new();
    fs.fail(FsOp::IsWritable, code, 1);
    assert_eq!(write_access(&fs, &path).unwrap_err(), os);
    assert_eq!(write_access(&fs, &path).unwrap(), WriteAccess::Writable);

    // "Allow editing" clears the attribute and then verifies the file is writable.
    let fs = FaultFs::new();
    fs.fail(FsOp::IsWritable, code, 1);
    assert_eq!(allow_editing(&fs, &path).unwrap_err(), os);
    allow_editing(&fs, &path).unwrap();

    RealFs
        .set_attributes(&path, Attributes(Attributes::READONLY))
        .unwrap();
    let fs = FaultFs::new();
    fs.fail(FsOp::SetAttributes, code, 1);
    assert_eq!(allow_editing(&fs, &path).unwrap_err(), os);
    assert!(RealFs.metadata(&path).unwrap().attrs.is_readonly());
    allow_editing(&fs, &path).unwrap();
    assert!(!RealFs.metadata(&path).unwrap().attrs.is_readonly());
}

#[test]
fn a_save_whose_stamp_cannot_be_read_back_still_wrote_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stamp.md");
    // The first metadata call is the existence check (a scripted "not found" like the real
    // one), the second the stamp query after the file was moved into place.
    let fs = FaultFs::new();
    fs.fail_for_path(FsOp::Metadata, &path, FsFailure::NotFound, 2);
    let error = save_atomic(&fs, &RecordingSleeper::default(), &path, b"written").unwrap_err();
    assert_eq!(error, SaveError::Fs(FsError::NotFound(path.clone())));
    assert_eq!(std::fs::read(&path).unwrap(), b"written");
    assert_eq!(names(dir.path()), ["stamp.md"], "no temp file is left");
}

/// Denies write access to Everyone on `path`; the guard restores it so the folder can be removed.
struct DenyWrite<'a>(&'a Path);

impl<'a> DenyWrite<'a> {
    fn new(path: &'a Path) -> Self {
        let status = Command::new("icacls")
            .arg(path)
            .args(["/deny", "*S-1-1-0:(W)"])
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "icacls: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        Self(path)
    }
}

impl Drop for DenyWrite<'_> {
    fn drop(&mut self) {
        let _ = Command::new("icacls")
            .arg(self.0)
            .args(["/remove:d", "*S-1-1-0"])
            .output();
    }
}

#[test]
fn acl_denied_files_report_access_denied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("acl.md");
    std::fs::write(&path, b"x").unwrap();
    let _deny = DenyWrite::new(&path);
    assert_eq!(
        write_access(&RealFs, &path).unwrap(),
        WriteAccess::AccessDenied
    );
    assert_eq!(
        allow_editing(&RealFs, &path).unwrap_err(),
        FsError::AccessDenied(path.clone())
    );
}

#[test]
fn sharing_violations_are_retried_with_backoff() {
    assert_eq!(
        SHARING_BACKOFF.map(|d| d.as_millis()),
        [50, 100, 200, 400, 800]
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.md");
    std::fs::write(&path, b"old").unwrap();
    for (failures, expected) in [
        (0, vec![]),
        (3, vec![50, 100, 200]),
        (5, vec![50, 100, 200, 400, 800]),
    ] {
        let fs = FaultFs::new();
        fs.fail(FsOp::ReplaceFile, FsFailure::SharingViolation, failures);
        let sleeper = RecordingSleeper::default();
        save_atomic(&fs, &sleeper, &path, b"new").unwrap();
        assert_eq!(sleeper.millis(), expected, "{failures} failures");
        assert_eq!(fs.calls(FsOp::ReplaceFile), failures + 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::write(&path, b"old").unwrap();
    }
    // Six failures exhaust the backoff: banner error, original intact, temp removed.
    let fs = FaultFs::new();
    fs.fail(FsOp::ReplaceFile, FsFailure::SharingViolation, 6);
    let sleeper = RecordingSleeper::default();
    let error = save_atomic(&fs, &sleeper, &path, b"new").unwrap_err();
    assert_eq!(error, SaveError::SharingViolation(path.clone()));
    assert!(error.to_string().contains("in use"));
    assert_eq!(sleeper.millis(), [50, 100, 200, 400, 800]);
    assert_eq!(std::fs::read(&path).unwrap(), b"old");
    assert_eq!(names(dir.path()), ["busy.md"]);
    assert_eq!(fs.calls(FsOp::DeleteFile), 1);
}

#[test]
fn new_files_are_renamed_into_place_with_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.md");
    let fs = FaultFs::new();
    fs.fail(FsOp::Rename, FsFailure::SharingViolation, 2);
    let sleeper = RecordingSleeper::default();
    save_atomic(&fs, &sleeper, &path, b"data").unwrap();
    assert_eq!(sleeper.millis(), [50, 100]);
    assert_eq!(std::fs::read(&path).unwrap(), b"data");
    assert_eq!(fs.calls(FsOp::ReplaceFile), 0);
}

#[test]
fn replace_failure_falls_back_to_move_with_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("share.md");
    std::fs::write(&path, b"old").unwrap();
    RealFs
        .set_attributes(&path, Attributes(Attributes::HIDDEN))
        .unwrap();
    let fs = FaultFs::new();
    fs.fail(FsOp::ReplaceFile, FsFailure::UnableToMoveReplacement, 1);
    let sleeper = RecordingSleeper::default();
    let stamp = save_atomic(&fs, &sleeper, &path, b"via move").unwrap();
    assert_eq!(fs.calls(FsOp::MoveReplace), 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"via move");
    assert!(
        RealFs.metadata(&path).unwrap().attrs.is_hidden(),
        "attributes carried over"
    );
    assert_eq!(stamp.size, 8);
    assert_eq!(names(dir.path()), ["share.md"]);

    // The fallback retries sharing violations too.
    fs.fail(FsOp::ReplaceFile, FsFailure::UnableToMoveReplacement, 1);
    fs.fail(FsOp::MoveReplace, FsFailure::SharingViolation, 1);
    save_atomic(&fs, &sleeper, &path, b"again").unwrap();
    assert_eq!(sleeper.millis(), [50]);

    // Fallback failure → error, temp file removed.
    fs.fail(FsOp::ReplaceFile, FsFailure::UnableToMoveReplacement, 1);
    fs.fail(FsOp::MoveReplace, FsFailure::AccessDenied, 1);
    let error = save_atomic(&fs, &sleeper, &path, b"never").unwrap_err();
    assert_eq!(error, SaveError::AccessDenied(path.clone()));
    assert_eq!(std::fs::read(&path).unwrap(), b"again");
    assert_eq!(names(dir.path()), ["share.md"]);

    // Attribute transfer failing aborts the fallback.
    fs.fail(FsOp::ReplaceFile, FsFailure::UnableToMoveReplacement, 1);
    fs.fail(FsOp::SetAttributes, FsFailure::AccessDenied, 1);
    assert!(matches!(
        save_atomic(&fs, &sleeper, &path, b"never").unwrap_err(),
        SaveError::AccessDenied(_)
    ));
    assert_eq!(names(dir.path()), ["share.md"]);
}

#[test]
fn other_failures_are_classified_and_clean_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.md");
    std::fs::write(&path, b"old").unwrap();
    let sleeper = RecordingSleeper::default();

    let fs = FaultFs::new();
    fs.fail(FsOp::WriteNew, FsFailure::DiskFull, 1);
    let error = save_atomic(&fs, &sleeper, &path, b"x").unwrap_err();
    assert!(matches!(error, SaveError::DiskFull(_)));
    assert!(error.to_string().contains("disk is full"));
    assert_eq!(fs.calls(FsOp::DeleteFile), 1, "partial temp file removed");

    let fs = FaultFs::new();
    fs.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    let error = save_atomic(&fs, &sleeper, &path, b"x").unwrap_err();
    assert!(matches!(error, SaveError::AccessDenied(_)));
    assert!(error.to_string().contains("access denied"));

    let fs = FaultFs::new();
    fs.fail(FsOp::Metadata, FsFailure::Code(1234), 1);
    assert!(matches!(
        save_atomic(&fs, &sleeper, &path, b"x").unwrap_err(),
        SaveError::Fs(FsError::Os { code: 1234, .. })
    ));

    let fs = FaultFs::new();
    fs.fail(FsOp::ReplaceFile, FsFailure::Code(1234), 1);
    assert!(matches!(
        save_atomic(&fs, &sleeper, &path, b"x").unwrap_err(),
        SaveError::Fs(FsError::Os { code: 1234, .. })
    ));
    assert_eq!(names(dir.path()), ["f.md"]);

    // The stamp query after the write failing is reported too.
    let fs = FaultFs::new();
    fs.fail(FsOp::Metadata, FsFailure::NotFound, 1);
    let fresh = dir.path().join("fresh.md");
    assert_eq!(save_atomic(&fs, &sleeper, &fresh, b"ok").unwrap().size, 2);
    let fs = FaultFs::new();
    let late = dir.path().join("late.md");
    fs.fail_for_path(FsOp::Metadata, &late, FsFailure::NotFound, 1);
    // First metadata call (existence check) is consumed by the NotFound script, the second
    // (stamp) is real.
    assert_eq!(save_atomic(&fs, &sleeper, &late, b"zz").unwrap().size, 2);
}

#[test]
fn temp_name_collisions_pick_another_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.md");
    let sleeper = RecordingSleeper::default();
    let fs = FaultFs::new();
    fs.fail(
        FsOp::WriteNew,
        FsFailure::Code(ssw_io::fs::win32::ALREADY_EXISTS),
        2,
    );
    save_atomic(&fs, &sleeper, &path, b"x").unwrap();
    assert_eq!(fs.calls(FsOp::WriteNew), 3);

    let fs = FaultFs::new();
    fs.fail_always(
        FsOp::WriteNew,
        FsFailure::Code(ssw_io::fs::win32::ALREADY_EXISTS),
    );
    assert!(matches!(
        save_atomic(&fs, &sleeper, &path, b"y").unwrap_err(),
        SaveError::Fs(FsError::AlreadyExists(_))
    ));
    assert_eq!(fs.calls(FsOp::WriteNew), 8);
    assert_eq!(std::fs::read(&path).unwrap(), b"x");
}

#[test]
fn thread_sleeper_blocks_for_the_duration() {
    let started = Instant::now();
    ThreadSleeper.sleep(Duration::from_millis(20));
    assert!(started.elapsed() >= Duration::from_millis(19));
}
