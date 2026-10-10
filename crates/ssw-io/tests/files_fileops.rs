use std::path::{Path, PathBuf};

use ssw_io::fileops::{
    self, FileOpError, MAX_NAME_UTF16, NameError, NameStyle, copy_into, copy_tree, duplicate,
    free_name, move_into, new_file, new_folder, new_untitled_file, new_untitled_folder,
    reveal_in_explorer, validate_name,
};
use ssw_io::fs::{FaultFs, FileSystem, FsError, FsFailure, FsOp, RealFs, win32};

mod files_common;
use files_common::RecycleBinGuard;

fn touch(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
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

#[test]
fn valid_names_pass() {
    let longest = "a".repeat(MAX_NAME_UTF16);
    for name in [
        "Notes.md",
        "a",
        " leading space.md",
        "İstanbul ışık.md",
        "emoji 😀.md",
        "CONSOLE.md",
        "COM0.txt",
        "COM10",
        "nul-ish.md",
        "dots.in.name.md",
        ".hidden",
        longest.as_str(),
    ] {
        assert_eq!(validate_name(name), Ok(()), "{name:?}");
    }
}

#[test]
fn invalid_names_name_the_violated_rule() {
    let too_long = "a".repeat(MAX_NAME_UTF16 + 1);
    // 128 astral characters are 256 UTF-16 units.
    let too_long_astral = "😀".repeat(128);
    let table: Vec<(&str, NameError)> = vec![
        ("", NameError::Empty),
        ("   ", NameError::Empty),
        (too_long.as_str(), NameError::TooLong),
        (too_long_astral.as_str(), NameError::TooLong),
        ("a<b", NameError::InvalidChar('<')),
        ("a>b", NameError::InvalidChar('>')),
        ("a:b", NameError::InvalidChar(':')),
        ("a\"b", NameError::InvalidChar('"')),
        ("a/b", NameError::InvalidChar('/')),
        ("a\\b", NameError::InvalidChar('\\')),
        ("a|b", NameError::InvalidChar('|')),
        ("a?b", NameError::InvalidChar('?')),
        ("a*b", NameError::InvalidChar('*')),
        ("tab\there", NameError::InvalidChar('\t')),
        ("nul\u{1}", NameError::InvalidChar('\u{1}')),
        ("name.", NameError::TrailingDotOrSpace),
        ("name ", NameError::TrailingDotOrSpace),
        (".", NameError::TrailingDotOrSpace),
        ("..", NameError::TrailingDotOrSpace),
        ("con", NameError::Reserved("CON".into())),
        ("CON", NameError::Reserved("CON".into())),
        ("Con.txt", NameError::Reserved("CON".into())),
        ("nul.tar.gz", NameError::Reserved("NUL".into())),
        ("aux", NameError::Reserved("AUX".into())),
        ("PRN.md", NameError::Reserved("PRN".into())),
        ("com1", NameError::Reserved("COM1".into())),
        ("COM9.md", NameError::Reserved("COM9".into())),
        ("lpt1", NameError::Reserved("LPT1".into())),
        ("LPT9.txt", NameError::Reserved("LPT9".into())),
        ("LPT1 .txt", NameError::Reserved("LPT1".into())),
    ];
    for (name, expected) in table {
        assert_eq!(validate_name(name), Err(expected), "{name:?}");
    }
}

#[test]
fn name_errors_have_readable_messages() {
    assert_eq!(NameError::Empty.to_string(), "the name is empty");
    assert_eq!(NameError::TooLong.to_string(), "the name is too long");
    assert_eq!(
        NameError::InvalidChar('?').to_string(),
        "the name contains the invalid character '?'"
    );
    assert_eq!(
        NameError::Reserved("CON".into()).to_string(),
        "CON is a reserved name"
    );
    assert_eq!(
        NameError::TrailingDotOrSpace.to_string(),
        "the name ends with a dot or a space"
    );
    let inside = FileOpError::IntoSelf(PathBuf::from("C:\\x"));
    assert_eq!(
        inside.to_string(),
        "cannot place a folder inside itself: C:\\x"
    );
}

#[test]
fn free_names_follow_each_style() {
    let dir = tempfile::tempdir().unwrap();
    let free =
        |stem: &str, ext: &str, style| free_name(&RealFs, dir.path(), stem, ext, style).unwrap();
    assert_eq!(free("Untitled", "md", NameStyle::Numbered), "Untitled.md");
    assert_eq!(free("note", "md", NameStyle::Copy), "note copy.md");
    assert_eq!(free("note", "md", NameStyle::Conflict), "note.md");

    touch(&dir.path().join("Untitled.md"), "");
    touch(&dir.path().join("note.md"), "");
    touch(&dir.path().join("note copy.md"), "");
    assert_eq!(free("Untitled", "md", NameStyle::Numbered), "Untitled 2.md");
    assert_eq!(free("note", "md", NameStyle::Copy), "note copy 2.md");
    assert_eq!(free("note", "md", NameStyle::Conflict), "note (2).md");

    touch(&dir.path().join("note (2).md"), "");
    touch(&dir.path().join("note copy 2.md"), "");
    assert_eq!(free("note", "md", NameStyle::Conflict), "note (3).md");
    assert_eq!(free("note", "md", NameStyle::Copy), "note copy 3.md");
    // Folders and extensionless files have no extension to append.
    std::fs::create_dir(dir.path().join("New folder")).unwrap();
    assert_eq!(free("New folder", "", NameStyle::Numbered), "New folder 2");
    // Names match case-insensitively like NTFS does.
    touch(&dir.path().join("CASE.md"), "");
    assert_eq!(free("case", "md", NameStyle::Conflict), "case (2).md");
}

#[test]
fn free_name_stops_at_unexpected_errors() {
    let dir = tempfile::tempdir().unwrap();
    let fault = FaultFs::new();
    fault.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    assert!(matches!(
        free_name(&fault, dir.path(), "a", "md", NameStyle::Numbered),
        Err(FsError::AccessDenied(_))
    ));
}

#[test]
fn rename_changes_the_name_inside_the_same_folder() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.md");
    touch(&path, "content");
    let renamed = fileops::rename(&RealFs, &path, "b.md").unwrap();
    assert_eq!(renamed, dir.path().join("b.md"));
    assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "content");
    assert_eq!(names(dir.path()), ["b.md"]);

    // The same name is a no-op.
    assert_eq!(fileops::rename(&RealFs, &renamed, "b.md").unwrap(), renamed);
    assert_eq!(names(dir.path()), ["b.md"]);

    // A case-only change really renames.
    let upper = fileops::rename(&RealFs, &renamed, "B.md").unwrap();
    assert_eq!(names(dir.path()), ["B.md"]);
    assert_eq!(std::fs::read_to_string(upper).unwrap(), "content");
}

#[test]
fn rename_validates_names_and_refuses_to_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.md");
    touch(&path, "a");
    touch(&dir.path().join("taken.md"), "taken");
    assert_eq!(
        fileops::rename(&RealFs, &path, "con.md"),
        Err(FileOpError::Name(NameError::Reserved("CON".into())))
    );
    assert_eq!(
        fileops::rename(&RealFs, &path, "a/b.md"),
        Err(FileOpError::Name(NameError::InvalidChar('/')))
    );
    assert_eq!(
        fileops::rename(&RealFs, &path, "taken.md"),
        Err(FileOpError::Fs(FsError::AlreadyExists(
            dir.path().join("taken.md")
        )))
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("taken.md")).unwrap(),
        "taken"
    );
    assert_eq!(names(dir.path()), ["a.md", "taken.md"]);
    let missing = dir.path().join("missing.md");
    assert!(matches!(
        fileops::rename(&RealFs, &missing, "x.md"),
        Err(FileOpError::Fs(FsError::NotFound(_)))
    ));
}

#[test]
fn new_files_and_folders() {
    let dir = tempfile::tempdir().unwrap();
    let file = new_file(&RealFs, dir.path(), "note.md", b"# Hi\n").unwrap();
    assert_eq!(file, dir.path().join("note.md"));
    assert_eq!(std::fs::read(&file).unwrap(), b"# Hi\n");
    assert_eq!(
        new_file(&RealFs, dir.path(), "note.md", b"other"),
        Err(FileOpError::Fs(FsError::AlreadyExists(file.clone())))
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"# Hi\n");
    assert_eq!(
        new_file(&RealFs, dir.path(), "bad?.md", b""),
        Err(FileOpError::Name(NameError::InvalidChar('?')))
    );

    let folder = new_folder(&RealFs, dir.path(), "Notes").unwrap();
    assert!(folder.is_dir());
    assert_eq!(
        new_folder(&RealFs, dir.path(), "Notes"),
        Err(FileOpError::Fs(FsError::AlreadyExists(folder.clone())))
    );
    assert_eq!(
        new_folder(&RealFs, dir.path(), "NUL"),
        Err(FileOpError::Name(NameError::Reserved("NUL".into())))
    );
    let missing_parent = dir.path().join("missing");
    assert!(matches!(
        new_folder(&RealFs, &missing_parent, "x"),
        Err(FileOpError::Fs(FsError::NotFound(_)))
    ));
}

#[test]
fn untitled_files_and_folders_count_up() {
    let dir = tempfile::tempdir().unwrap();
    let first = new_untitled_file(&RealFs, dir.path(), "md").unwrap();
    let second = new_untitled_file(&RealFs, dir.path(), "md").unwrap();
    let text = new_untitled_file(&RealFs, dir.path(), "txt").unwrap();
    assert_eq!(first, dir.path().join("Untitled.md"));
    assert_eq!(second, dir.path().join("Untitled 2.md"));
    assert_eq!(text, dir.path().join("Untitled.txt"));
    assert_eq!(std::fs::metadata(&first).unwrap().len(), 0);

    let folder = new_untitled_folder(&RealFs, dir.path(), "New folder").unwrap();
    let next = new_untitled_folder(&RealFs, dir.path(), "New folder").unwrap();
    assert_eq!(folder, dir.path().join("New folder"));
    assert_eq!(next, dir.path().join("New folder 2"));
    assert!(next.is_dir());
    let fault = FaultFs::new();
    fault.fail(FsOp::Metadata, FsFailure::SharingViolation, 1);
    assert!(matches!(
        new_untitled_file(&fault, dir.path(), "md"),
        Err(FileOpError::Fs(FsError::SharingViolation(_)))
    ));
    fault.fail(FsOp::Metadata, FsFailure::SharingViolation, 1);
    assert!(matches!(
        new_untitled_folder(&fault, dir.path(), "New folder"),
        Err(FileOpError::Fs(FsError::SharingViolation(_)))
    ));
}

fn sample_tree(root: &Path) {
    touch(&root.join("top.md"), "top");
    touch(&root.join("a").join("one.md"), "one");
    touch(&root.join("a").join("b").join("two.txt"), "two");
    std::fs::create_dir_all(root.join("empty")).unwrap();
}

fn tree_listing(root: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in RealFs.list_dir(dir).unwrap() {
            let path = dir.join(&entry.name);
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if entry.meta.is_dir() {
                out.push(format!("{relative}\\"));
                walk(base, &path, out);
            } else {
                out.push(format!(
                    "{relative}={}",
                    std::fs::read_to_string(&path).unwrap()
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn copy_and_remove_trees_recursively() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    sample_tree(&source);
    let copy = dir.path().join("copy");
    copy_tree(&RealFs, &source, &copy).unwrap();
    assert_eq!(tree_listing(&copy), tree_listing(&source));
    assert_eq!(
        tree_listing(&copy),
        [
            "a\\",
            "a\\b\\",
            "a\\b\\two.txt=two",
            "a\\one.md=one",
            "empty\\",
            "top.md=top"
        ]
    );

    assert_eq!(
        copy_tree(&RealFs, &source, &copy),
        Err(FsError::AlreadyExists(copy.clone()))
    );
    let ghost = dir.path().join("ghost");
    assert!(matches!(
        copy_tree(&RealFs, &ghost, &dir.path().join("ghost-copy")),
        Err(FsError::NotFound(_))
    ));

    fileops::remove_tree(&RealFs, &copy).unwrap();
    assert!(!copy.exists());
    assert!(source.join("a").join("b").join("two.txt").exists());
    assert!(matches!(
        fileops::remove_tree(&RealFs, &copy),
        Err(FsError::NotFound(_))
    ));
}

#[test]
fn tree_operations_stop_at_the_first_failing_call() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("src");
    sample_tree(&source);
    let fault = FaultFs::new();

    fault.fail(FsOp::CopyFile, FsFailure::DiskFull, 1);
    assert!(matches!(
        copy_tree(&fault, &source, &dir.path().join("copy")),
        Err(FsError::DiskFull(_))
    ));
    fault.clear();

    fault.fail(FsOp::DeleteFile, FsFailure::AccessDenied, 1);
    assert!(matches!(
        fileops::remove_tree(&fault, &source),
        Err(FsError::AccessDenied(_))
    ));
    assert!(
        source.exists(),
        "the folder is still there after the failure"
    );
    fault.clear();

    fault.fail(FsOp::RemoveDir, FsFailure::SharingViolation, 1);
    assert!(matches!(
        fileops::remove_tree(&fault, &source),
        Err(FsError::SharingViolation(_))
    ));
    fault.clear();
    fileops::remove_tree(&fault, &source).unwrap();
    assert!(!source.exists());
}

#[test]
fn duplicate_names_the_copy_next_to_the_original() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("doc.md"), "doc");
    touch(&dir.path().join("a.b.md"), "dotted");
    touch(&dir.path().join("README"), "readme");
    let first = duplicate(&RealFs, &dir.path().join("doc.md")).unwrap();
    let second = duplicate(&RealFs, &dir.path().join("doc.md")).unwrap();
    assert_eq!(first, dir.path().join("doc copy.md"));
    assert_eq!(second, dir.path().join("doc copy 2.md"));
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "doc");
    assert_eq!(
        duplicate(&RealFs, &dir.path().join("a.b.md")).unwrap(),
        dir.path().join("a.b copy.md")
    );
    assert_eq!(
        duplicate(&RealFs, &dir.path().join("README")).unwrap(),
        dir.path().join("README copy")
    );
    // Duplicating a copy keeps counting from its own name.
    assert_eq!(
        duplicate(&RealFs, &first).unwrap(),
        dir.path().join("doc copy copy.md")
    );
}

#[test]
fn duplicating_a_folder_copies_everything_below_it() {
    let dir = tempfile::tempdir().unwrap();
    sample_tree(&dir.path().join("notes"));
    sample_tree(&dir.path().join("v1.2"));
    let copy = duplicate(&RealFs, &dir.path().join("notes")).unwrap();
    let again = duplicate(&RealFs, &dir.path().join("notes")).unwrap();
    assert_eq!(copy, dir.path().join("notes copy"));
    assert_eq!(again, dir.path().join("notes copy 2"));
    assert_eq!(tree_listing(&copy), tree_listing(&dir.path().join("notes")));
    // A dot in a folder name is not an extension.
    assert_eq!(
        duplicate(&RealFs, &dir.path().join("v1.2")).unwrap(),
        dir.path().join("v1.2 copy")
    );
}

#[test]
fn duplicate_reports_failures() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("doc.md"), "doc");
    assert!(matches!(
        duplicate(&RealFs, &dir.path().join("missing.md")),
        Err(FileOpError::Fs(FsError::NotFound(_)))
    ));
    let fault = FaultFs::new();
    fault.fail(FsOp::CopyFile, FsFailure::DiskFull, 1);
    assert!(matches!(
        duplicate(&fault, &dir.path().join("doc.md")),
        Err(FileOpError::Fs(FsError::DiskFull(_)))
    ));
    assert_eq!(names(dir.path()), ["doc.md"]);
}

#[test]
fn copy_into_resolves_name_clashes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    touch(&dir.path().join("a.md"), "source");
    touch(&dir.path().join("README"), "readme");

    let first = copy_into(&RealFs, &dir.path().join("a.md"), &out).unwrap();
    let second = copy_into(&RealFs, &dir.path().join("a.md"), &out).unwrap();
    let third = copy_into(&RealFs, &dir.path().join("a.md"), &out).unwrap();
    assert_eq!(first, out.join("a.md"));
    assert_eq!(second, out.join("a (2).md"));
    assert_eq!(third, out.join("a (3).md"));
    assert_eq!(std::fs::read_to_string(&third).unwrap(), "source");
    assert!(
        dir.path().join("a.md").exists(),
        "a copy leaves the original"
    );

    copy_into(&RealFs, &dir.path().join("README"), &out).unwrap();
    assert_eq!(
        copy_into(&RealFs, &dir.path().join("README"), &out).unwrap(),
        out.join("README (2)")
    );
}

#[test]
fn copy_into_copies_folders_but_not_into_themselves() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    sample_tree(&dir.path().join("docs"));
    let copied = copy_into(&RealFs, &dir.path().join("docs"), &out).unwrap();
    let clash = copy_into(&RealFs, &dir.path().join("docs"), &out).unwrap();
    assert_eq!(copied, out.join("docs"));
    assert_eq!(clash, out.join("docs (2)"));
    assert_eq!(
        tree_listing(&copied),
        tree_listing(&dir.path().join("docs"))
    );

    let docs = dir.path().join("docs");
    let into_self = Err(FileOpError::IntoSelf(docs.clone()));
    assert_eq!(copy_into(&RealFs, &docs, &docs), into_self);
    assert_eq!(copy_into(&RealFs, &docs, &docs.join("a")), into_self);
    assert_eq!(
        copy_into(&RealFs, &docs, &docs.join("a").join("b")),
        into_self
    );
    let shouting = PathBuf::from(docs.to_string_lossy().to_uppercase()).join("A");
    assert_eq!(copy_into(&RealFs, &docs, &shouting), into_self);
    assert!(!docs.join("a").join("docs").exists());

    // A sibling whose name merely starts with the same letters is a different folder.
    let sibling = dir.path().join("docs-archive");
    std::fs::create_dir(&sibling).unwrap();
    assert_eq!(
        copy_into(&RealFs, &docs, &sibling).unwrap(),
        sibling.join("docs")
    );
    assert!(matches!(
        copy_into(&RealFs, &dir.path().join("missing"), &out),
        Err(FileOpError::Fs(FsError::NotFound(_)))
    ));
}

#[test]
fn move_into_moves_files_and_resolves_clashes() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    touch(&dir.path().join("a.md"), "first");
    let moved = move_into(&RealFs, &dir.path().join("a.md"), &out).unwrap();
    assert_eq!(moved, out.join("a.md"));
    assert!(!dir.path().join("a.md").exists());
    assert_eq!(std::fs::read_to_string(&moved).unwrap(), "first");

    touch(&dir.path().join("a.md"), "second");
    let clash = move_into(&RealFs, &dir.path().join("a.md"), &out).unwrap();
    assert_eq!(clash, out.join("a (2).md"));
    assert_eq!(std::fs::read_to_string(out.join("a.md")).unwrap(), "first");
    assert_eq!(std::fs::read_to_string(&clash).unwrap(), "second");
}

#[test]
fn moving_into_the_current_folder_does_nothing() {
    let dir = tempfile::tempdir().unwrap();
    touch(&dir.path().join("a.md"), "a");
    let same = move_into(&RealFs, &dir.path().join("a.md"), dir.path()).unwrap();
    assert_eq!(same, dir.path().join("a.md"));
    assert_eq!(names(dir.path()), ["a.md"]);
}

#[test]
fn move_into_moves_folders_but_not_into_themselves() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    sample_tree(&dir.path().join("docs"));
    let before = tree_listing(&dir.path().join("docs"));
    let docs = dir.path().join("docs");

    let into_self = Err(FileOpError::IntoSelf(docs.clone()));
    assert_eq!(move_into(&RealFs, &docs, &docs), into_self);
    assert_eq!(move_into(&RealFs, &docs, &docs.join("a")), into_self);
    assert_eq!(tree_listing(&docs), before, "nothing moved");

    let moved = move_into(&RealFs, &docs, &out).unwrap();
    assert_eq!(moved, out.join("docs"));
    assert!(!docs.exists());
    assert_eq!(tree_listing(&moved), before);
    assert!(matches!(
        move_into(&RealFs, &docs, &out),
        Err(FileOpError::Fs(FsError::NotFound(_)))
    ));
}

#[test]
fn folders_that_cannot_be_renamed_across_volumes_are_copied_and_removed() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    sample_tree(&dir.path().join("docs"));
    let before = tree_listing(&dir.path().join("docs"));
    let fault = FaultFs::new();
    fault.fail(FsOp::Rename, FsFailure::Code(win32::NOT_SAME_DEVICE), 1);
    let moved = move_into(&fault, &dir.path().join("docs"), &out).unwrap();
    assert_eq!(moved, out.join("docs"));
    assert_eq!(tree_listing(&moved), before);
    assert!(!dir.path().join("docs").exists());
    assert_eq!(fault.calls(FsOp::Rename), 1);
}

#[test]
fn other_rename_failures_are_passed_on_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out");
    std::fs::create_dir(&out).unwrap();
    touch(&dir.path().join("a.md"), "a");
    let fault = FaultFs::new();
    fault.fail(FsOp::Rename, FsFailure::AccessDenied, 1);
    assert_eq!(
        move_into(&fault, &dir.path().join("a.md"), &out),
        Err(FileOpError::Fs(FsError::AccessDenied(out.join("a.md"))))
    );
    assert!(dir.path().join("a.md").exists());
    fault.fail(FsOp::Rename, FsFailure::Code(1234), 1);
    assert_eq!(
        move_into(&fault, &dir.path().join("a.md"), &out),
        Err(FileOpError::Fs(FsError::Os {
            code: 1234,
            path: out.join("a.md")
        }))
    );
}

#[test]
fn recycling_removes_files_and_folders_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let bin = RecycleBinGuard::new();
    let file = dir.path().join(bin.name("-recycle-me.md"));
    touch(&file, "bye");
    let folder = dir.path().join(bin.name("-recycle-folder"));
    sample_tree(&folder);
    let keep = dir.path().join("keep.md");
    touch(&keep, "stay");

    fileops::recycle(&RealFs, &[file.clone(), folder.clone()]).unwrap();
    assert!(!file.exists());
    assert!(!folder.exists());
    assert!(keep.exists());
    fileops::recycle(&RealFs, &[]).unwrap();
    // Remove our two items from the Recycle Bin again; they were really in it.
    assert_eq!(bin.purge(), 2);
}

#[test]
fn recycling_reports_missing_items_and_failures() {
    let dir = tempfile::tempdir().unwrap();
    let ghost = dir.path().join("ghost.md");
    assert!(matches!(
        fileops::recycle(&RealFs, &[ghost]),
        Err(FsError::NotFound(_))
    ));
    let file = dir.path().join("stays.md");
    touch(&file, "x");
    let fault = FaultFs::new();
    fault.fail(FsOp::Recycle, FsFailure::AccessDenied, 1);
    assert_eq!(
        fileops::recycle(&fault, std::slice::from_ref(&file)),
        Err(FsError::AccessDenied(file.clone()))
    );
    assert!(file.exists());
}

#[test]
fn revealing_a_missing_path_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let ghost = dir.path().join("not-there").join("ghost.md");
    assert_eq!(
        reveal_in_explorer(&ghost),
        Err(FsError::NotFound(ghost.clone()))
    );
}
