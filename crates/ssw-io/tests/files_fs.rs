use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ssw_io::fs::{
    Attributes, FaultFs, FileMeta, FileSystem, FileTime, FsError, FsFailure, FsOp, RealFs, win32,
};

mod files_common;
use files_common::RecycleBinGuard;

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn names(fs: &dyn FileSystem, dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs
        .list_dir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    names
}

#[test]
fn win32_codes_are_classified() {
    let path = Path::new("C:\\x");
    let p = || path.to_path_buf();
    let cases = [
        (win32::FILE_NOT_FOUND, FsError::NotFound(p())),
        (win32::PATH_NOT_FOUND, FsError::NotFound(p())),
        (win32::BAD_NETPATH, FsError::NotFound(p())),
        (win32::ALREADY_EXISTS, FsError::AlreadyExists(p())),
        (win32::FILE_EXISTS, FsError::AlreadyExists(p())),
        (win32::ACCESS_DENIED, FsError::AccessDenied(p())),
        (win32::SHARING_VIOLATION, FsError::SharingViolation(p())),
        (win32::LOCK_VIOLATION, FsError::SharingViolation(p())),
        (win32::DISK_FULL, FsError::DiskFull(p())),
        (win32::HANDLE_DISK_FULL, FsError::DiskFull(p())),
        (win32::DIR_NOT_EMPTY, FsError::DirectoryNotEmpty(p())),
        (
            win32::UNABLE_TO_REMOVE_REPLACED,
            FsError::ReplaceUnsupported(p()),
        ),
        (
            win32::UNABLE_TO_MOVE_REPLACEMENT,
            FsError::ReplaceUnsupported(p()),
        ),
        (
            win32::UNABLE_TO_MOVE_REPLACEMENT_2,
            FsError::ReplaceUnsupported(p()),
        ),
        (win32::NOT_SUPPORTED, FsError::ReplaceUnsupported(p())),
        (win32::INVALID_NAME, FsError::InvalidName(p())),
        (
            4242,
            FsError::Os {
                code: 4242,
                path: p(),
            },
        ),
    ];
    for (code, expected) in cases {
        let error = FsError::from_code(code, path);
        assert_eq!(error, expected, "code {code}");
        assert_eq!(error.path(), path);
        assert!(error.to_string().contains("C:\\x"), "{error}");
    }
    assert_eq!(
        FsError::from_io(&io::Error::from_raw_os_error(2), path),
        FsError::NotFound(p())
    );
    assert_eq!(
        FsError::from_io(&io::Error::other("no os code"), path),
        FsError::Os { code: 0, path: p() }
    );
    let access = windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0005_u32 as i32));
    assert_eq!(
        FsError::from_windows(&access, path),
        FsError::AccessDenied(p())
    );
    let other = windows::core::Error::from_hresult(windows::core::HRESULT(0x8000_4005_u32 as i32));
    assert_eq!(
        FsError::from_windows(&other, path),
        FsError::Os {
            code: 0x8000_4005,
            path: p()
        }
    );
}

#[test]
fn file_time_converts_both_sides_of_1970() {
    let epoch = FileTime(116_444_736_000_000_000);
    assert_eq!(epoch.to_system_time(), UNIX_EPOCH);
    assert_eq!(FileTime::from_system_time(UNIX_EPOCH), epoch);
    assert_eq!(epoch.unix_millis(), 0);
    let later = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_700);
    let ticks = FileTime::from_system_time(later);
    assert_eq!(ticks.to_system_time(), later);
    assert_eq!(ticks.unix_millis(), 1_700_000_000_123);
    let earlier = UNIX_EPOCH - Duration::from_secs(10);
    let ticks = FileTime::from_system_time(earlier);
    assert_eq!(ticks, FileTime(116_444_736_000_000_000 - 100_000_000));
    assert_eq!(ticks.to_system_time(), earlier);
    assert_eq!(ticks.unix_millis(), -10_000);
    // 1601-01-01 is before the Unix epoch by 11 644 473 600 s.
    assert_eq!(
        FileTime(0).to_system_time(),
        UNIX_EPOCH - Duration::from_secs(11_644_473_600)
    );
    assert_eq!(
        FileTime::from_system_time(UNIX_EPOCH - Duration::from_secs(11_644_473_600)),
        FileTime(0)
    );
    // A corrupt timestamp above what `SystemTime` can hold clamps instead of overflowing.
    let latest = FileTime(i64::MAX.cast_unsigned());
    assert_eq!(FileTime(u64::MAX).to_system_time(), latest.to_system_time());
    assert_eq!(FileTime::from_system_time(latest.to_system_time()), latest);
}

#[test]
fn attributes_expose_their_bits() {
    let attrs = Attributes(0)
        .with(Attributes::READONLY)
        .with(Attributes::HIDDEN);
    assert!(attrs.is_readonly() && attrs.is_hidden());
    assert!(!attrs.is_system() && !attrs.is_dir() && !attrs.is_placeholder());
    let attrs = attrs
        .without(Attributes::READONLY)
        .with(Attributes::SYSTEM | Attributes::DIRECTORY);
    assert!(!attrs.is_readonly() && attrs.is_system() && attrs.is_dir());
    for bit in [
        Attributes::OFFLINE,
        Attributes::RECALL_ON_OPEN,
        Attributes::RECALL_ON_DATA_ACCESS,
    ] {
        assert!(Attributes(bit).is_placeholder(), "{bit:#x}");
    }
    assert!(!Attributes(Attributes::ARCHIVE | Attributes::REPARSE_POINT).is_placeholder());
    let meta = FileMeta {
        attrs: Attributes(Attributes::DIRECTORY | Attributes::OFFLINE),
        ..FileMeta::default()
    };
    assert!(meta.is_dir() && meta.is_placeholder());
    assert!(!FileMeta::default().is_dir());
}

#[test]
fn real_fs_files_and_metadata() {
    let dir = temp();
    let fs = RealFs;
    let file = dir.path().join("a.txt");
    fs.write_new(&file, b"hello").unwrap();
    assert_eq!(fs.read(&file).unwrap(), b"hello");
    assert!(matches!(
        fs.write_new(&file, b"x"),
        Err(FsError::AlreadyExists(_))
    ));
    let meta = fs.metadata(&file).unwrap();
    assert_eq!(meta.size, 5);
    assert!(!meta.is_dir() && !meta.is_placeholder());
    let age = SystemTime::now()
        .duration_since(meta.modified.to_system_time())
        .unwrap_or_default();
    assert!(age < Duration::from_secs(3600), "mtime {age:?} ago");
    assert!(fs.metadata(dir.path()).unwrap().is_dir());
    assert!(matches!(
        fs.metadata(&dir.path().join("nope")),
        Err(FsError::NotFound(_))
    ));
    assert!(matches!(
        fs.read(&dir.path().join("nope")),
        Err(FsError::NotFound(_))
    ));

    let copy = dir.path().join("b.txt");
    fs.copy_file(&file, &copy).unwrap();
    assert!(matches!(
        fs.copy_file(&file, &copy),
        Err(FsError::AlreadyExists(_))
    ));
    let moved = dir.path().join("c.txt");
    fs.rename(&copy, &moved).unwrap();
    assert!(matches!(
        fs.rename(&moved, &file),
        Err(FsError::AlreadyExists(_))
    ));
    assert_eq!(names(&fs, dir.path()), ["a.txt", "c.txt"]);
    fs.delete_file(&moved).unwrap();
    assert!(matches!(fs.delete_file(&moved), Err(FsError::NotFound(_))));
}

#[test]
fn real_fs_reads_at_most_the_requested_start_of_a_file() {
    let dir = temp();
    let fs = RealFs;
    let file = dir.path().join("a.bin");
    fs.write_new(&file, b"0123456789").unwrap();
    // Longer than the prefix: only the start comes back.
    assert_eq!(fs.read_prefix(&file, 4).unwrap(), b"0123");
    // Exactly as long, and shorter than the prefix: the whole file.
    assert_eq!(fs.read_prefix(&file, 10).unwrap(), b"0123456789");
    assert_eq!(fs.read_prefix(&file, 4096).unwrap(), b"0123456789");
    assert_eq!(fs.read_prefix(&file, 0).unwrap(), b"");
    let empty = dir.path().join("empty.bin");
    fs.write_new(&empty, b"").unwrap();
    assert_eq!(fs.read_prefix(&empty, 4).unwrap(), b"");
    assert!(matches!(
        fs.read_prefix(&dir.path().join("nope"), 4),
        Err(FsError::NotFound(_))
    ));
}

#[test]
fn real_fs_replace_keeps_attributes_and_move_replace_overwrites() {
    let dir = temp();
    let fs = RealFs;
    let target = dir.path().join("t.txt");
    let replacement = dir.path().join("r.txt");
    fs.write_new(&target, b"old").unwrap();
    fs.write_new(&replacement, b"new").unwrap();
    fs.set_attributes(&target, Attributes(Attributes::HIDDEN))
        .unwrap();
    fs.replace_file(&target, &replacement).unwrap();
    assert_eq!(fs.read(&target).unwrap(), b"new");
    assert!(fs.metadata(&target).unwrap().attrs.is_hidden());
    assert!(matches!(
        fs.metadata(&replacement),
        Err(FsError::NotFound(_))
    ));
    assert!(matches!(
        fs.replace_file(&target, &replacement),
        Err(FsError::NotFound(_))
    ));

    fs.write_new(&replacement, b"newer").unwrap();
    fs.move_replace(&replacement, &target).unwrap();
    assert_eq!(fs.read(&target).unwrap(), b"newer");
    assert!(matches!(
        fs.move_replace(&replacement, &target),
        Err(FsError::NotFound(_))
    ));
}

#[test]
fn real_fs_folders() {
    let dir = temp();
    let fs = RealFs;
    let nested = dir.path().join("x").join("y").join("z");
    fs.create_dir_all(&nested).unwrap();
    fs.create_dir_all(&nested).unwrap();
    assert!(matches!(
        fs.create_dir(&nested),
        Err(FsError::AlreadyExists(_))
    ));
    assert!(matches!(
        fs.create_dir(&dir.path().join("missing").join("child")),
        Err(FsError::NotFound(_))
    ));
    fs.create_dir(&dir.path().join("plain")).unwrap();
    fs.write_new(&dir.path().join("x").join("f.txt"), b"1")
        .unwrap();
    let listing = fs.list_dir(&dir.path().join("x")).unwrap();
    let mut seen: Vec<(String, bool)> = listing
        .iter()
        .map(|e| (e.name.clone(), e.meta.is_dir()))
        .collect();
    seen.sort();
    assert_eq!(seen, [("f.txt".to_owned(), false), ("y".to_owned(), true)]);
    assert_eq!(
        listing
            .iter()
            .find(|e| e.name == "f.txt")
            .unwrap()
            .meta
            .size,
        1
    );
    assert!(fs.list_dir(&dir.path().join("plain")).unwrap().is_empty());
    assert!(matches!(
        fs.list_dir(&dir.path().join("nope")),
        Err(FsError::NotFound(_))
    ));
    assert!(matches!(
        fs.remove_dir(&dir.path().join("x")),
        Err(FsError::DirectoryNotEmpty(_))
    ));
    fs.remove_dir(&dir.path().join("plain")).unwrap();
    assert!(matches!(
        fs.remove_dir(&dir.path().join("plain")),
        Err(FsError::NotFound(_))
    ));
}

#[test]
fn real_fs_attributes_and_write_access() {
    let dir = temp();
    let fs = RealFs;
    let file = dir.path().join("a.txt");
    fs.write_new(&file, b"1").unwrap();
    assert!(fs.is_writable(&file).unwrap());
    fs.set_attributes(&file, Attributes(Attributes::READONLY | Attributes::HIDDEN))
        .unwrap();
    let attrs = fs.metadata(&file).unwrap().attrs;
    assert!(attrs.is_readonly() && attrs.is_hidden());
    assert!(!fs.is_writable(&file).unwrap());
    // Unsettable bits are masked; an empty set clears everything.
    fs.set_attributes(&file, Attributes(Attributes::DIRECTORY))
        .unwrap();
    let attrs = fs.metadata(&file).unwrap().attrs;
    assert!(!attrs.is_readonly() && !attrs.is_hidden() && !attrs.is_dir());
    assert!(matches!(
        fs.is_writable(&dir.path().join("nope")),
        Err(FsError::NotFound(_))
    ));
    assert!(matches!(
        fs.set_attributes(&dir.path().join("nope"), Attributes(0)),
        Err(FsError::NotFound(_))
    ));
}

#[test]
fn real_fs_recycles_files_and_folders() {
    let dir = temp();
    let fs = RealFs;
    let bin = RecycleBinGuard::new();
    fs.recycle(&[]).unwrap();
    let file = dir.path().join(bin.name("-recycle.txt"));
    let folder = dir.path().join(bin.name("-recycle-folder"));
    fs.write_new(&file, b"bye").unwrap();
    fs.create_dir(&folder).unwrap();
    fs.write_new(&folder.join("inner.txt"), b"x").unwrap();
    fs.recycle(&[file.clone(), folder.clone()]).unwrap();
    assert!(matches!(fs.metadata(&file), Err(FsError::NotFound(_))));
    assert!(matches!(fs.metadata(&folder), Err(FsError::NotFound(_))));
    let missing = dir.path().join(bin.name("-never-existed.txt"));
    assert!(
        matches!(fs.recycle(std::slice::from_ref(&missing)), Err(FsError::NotFound(p)) if p == missing)
    );
    // Both items really went to the Recycle Bin (not deleted for good); remove them again.
    assert_eq!(bin.purge(), 2);
    assert_eq!(bin.purge(), 0, "nothing of this test is left in the bin");
}

fn run_every_op(fs: &FaultFs, dir: &Path) -> Vec<(FsOp, Result<(), FsError>)> {
    let file = dir.join("f.txt");
    let other = dir.join("g.txt");
    vec![
        (FsOp::Read, fs.read(&file).map(drop)),
        (FsOp::Metadata, fs.metadata(&file).map(drop)),
        (FsOp::WriteNew, fs.write_new(&file, b"x")),
        (FsOp::ReplaceFile, fs.replace_file(&file, &other)),
        (FsOp::MoveReplace, fs.move_replace(&other, &file)),
        (FsOp::Rename, fs.rename(&other, &file)),
        (FsOp::CopyFile, fs.copy_file(&other, &file)),
        (FsOp::DeleteFile, fs.delete_file(&other)),
        (FsOp::RemoveDir, fs.remove_dir(&other)),
        (FsOp::CreateDir, fs.create_dir(&dir.join("d"))),
        (FsOp::CreateDirAll, fs.create_dir_all(&dir.join("e"))),
        (FsOp::ListDir, fs.list_dir(dir).map(drop)),
        (FsOp::SetAttributes, fs.set_attributes(&file, Attributes(0))),
        (FsOp::IsWritable, fs.is_writable(&file).map(drop)),
        (FsOp::Recycle, fs.recycle(std::slice::from_ref(&other))),
    ]
}

#[test]
fn fault_fs_fails_every_operation_with_the_scripted_code() {
    let dir = temp();
    let ops = [
        FsOp::Read,
        FsOp::Metadata,
        FsOp::WriteNew,
        FsOp::ReplaceFile,
        FsOp::MoveReplace,
        FsOp::Rename,
        FsOp::CopyFile,
        FsOp::DeleteFile,
        FsOp::RemoveDir,
        FsOp::CreateDir,
        FsOp::CreateDirAll,
        FsOp::ListDir,
        FsOp::SetAttributes,
        FsOp::IsWritable,
        FsOp::Recycle,
    ];
    for op in ops {
        let fs = FaultFs::new();
        fs.fail(op, FsFailure::AccessDenied, 1);
        for (called, result) in run_every_op(&fs, dir.path()) {
            if called == op {
                assert!(
                    matches!(result, Err(FsError::AccessDenied(_))),
                    "{op:?}: {result:?}"
                );
            }
        }
        assert_eq!(fs.calls(op), 1, "{op:?}");
    }
}

#[test]
fn fault_fs_prefix_reads_fail_like_reads_and_otherwise_pass_through() {
    let dir = temp();
    let file = dir.path().join("f.txt");
    RealFs.write_new(&file, b"0123456789").unwrap();
    let fs = FaultFs::new();
    assert_eq!(fs.read_prefix(&file, 4).unwrap(), b"0123");
    assert_eq!(fs.calls(FsOp::Read), 1, "a prefix read counts as a read");
    // The scripted read failure hits the prefix read, once.
    fs.fail(FsOp::Read, FsFailure::AccessDenied, 1);
    assert!(matches!(
        fs.read_prefix(&file, 4),
        Err(FsError::AccessDenied(_))
    ));
    assert_eq!(fs.read_prefix(&file, 100).unwrap(), b"0123456789");
    assert_eq!(fs.calls(FsOp::Read), 3);
}

#[test]
fn fault_fs_scripts_count_filter_and_clear() {
    let dir = temp();
    let file = dir.path().join("f.txt");
    RealFs.write_new(&file, b"data").unwrap();
    let fs = FaultFs::new();
    assert_eq!(fs.calls(FsOp::Read), 0);
    // Two scripted failures, then the real call.
    fs.fail(FsOp::Read, FsFailure::SharingViolation, 2);
    assert!(matches!(fs.read(&file), Err(FsError::SharingViolation(_))));
    assert!(matches!(fs.read(&file), Err(FsError::SharingViolation(_))));
    assert_eq!(fs.read(&file).unwrap(), b"data");
    assert_eq!(fs.calls(FsOp::Read), 3);
    // Path filter: only the scripted path fails.
    let other = dir.path().join("o.txt");
    RealFs.write_new(&other, b"o").unwrap();
    fs.fail_for_path(FsOp::Read, &file, FsFailure::DiskFull, 5);
    assert_eq!(fs.read(&other).unwrap(), b"o");
    assert!(matches!(fs.read(&file), Err(FsError::DiskFull(_))));
    // Rules are consulted in order; a spent rule is skipped.
    fs.clear();
    assert_eq!(fs.read(&file).unwrap(), b"data");
    fs.fail_always(FsOp::Metadata, FsFailure::NotFound);
    for _ in 0..3 {
        assert!(matches!(fs.metadata(&file), Err(FsError::NotFound(_))));
    }
    fs.clear();
    fs.fail(FsOp::Read, FsFailure::Code(4242), 1);
    assert!(matches!(
        fs.read(&file),
        Err(FsError::Os { code: 4242, .. })
    ));
    fs.fail(FsOp::Read, FsFailure::UnableToMoveReplacement, 1);
    assert!(matches!(
        fs.read(&file),
        Err(FsError::ReplaceUnsupported(_))
    ));
    for (failure, code) in [
        (FsFailure::SharingViolation, win32::SHARING_VIOLATION),
        (FsFailure::AccessDenied, win32::ACCESS_DENIED),
        (FsFailure::DiskFull, win32::DISK_FULL),
        (
            FsFailure::UnableToMoveReplacement,
            win32::UNABLE_TO_MOVE_REPLACEMENT,
        ),
        (FsFailure::NotFound, win32::FILE_NOT_FOUND),
        (FsFailure::Code(7), 7),
    ] {
        assert_eq!(failure.code(), code);
    }
}
