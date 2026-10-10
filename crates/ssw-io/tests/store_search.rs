//! Search in folder: ignore rules, binary skipping, legacy encodings, options, limits,
//! cancellation.

use std::ops::Range;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use ssw_io::folder_search::{
    FileMatches, MAX_PREVIEW_BYTES, SearchError, SearchOptions, SearchSummary, search_folder,
};
use tempfile::{TempDir, tempdir};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
};
use windows::Win32::System::IO::OVERLAPPED;

fn write(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
    let path = rel
        .split('/')
        .fold(root.to_path_buf(), |p, part| p.join(part));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

fn fixture(name: &str) -> Vec<u8> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "..",
        "tests",
        "fixtures",
        "text",
        name,
    ]
    .iter()
    .collect();
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every search of this file goes through here, so that one instantiation of the generic
/// `search_folder` sees all of its outcomes: results, rejected patterns and cancellation.
fn try_search(
    root: &Path,
    options: &SearchOptions,
    placeholder: &dyn Fn(&Path) -> bool,
    cancel: &AtomicBool,
) -> Result<(Vec<FileMatches>, SearchSummary), SearchError> {
    let mut found = Vec::new();
    let summary = search_folder(root, options, placeholder, cancel, |file| {
        found.push(file);
    })?;
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((found, summary))
}

fn search_with(
    root: &Path,
    options: &SearchOptions,
    placeholder: &dyn Fn(&Path) -> bool,
) -> (Vec<FileMatches>, SearchSummary) {
    try_search(root, options, placeholder, &AtomicBool::new(false)).unwrap()
}

fn search(root: &Path, options: &SearchOptions) -> (Vec<FileMatches>, SearchSummary) {
    search_with(root, options, &|_| false)
}

fn lines(file: &FileMatches) -> Vec<u64> {
    file.matches.iter().map(|m| m.line).collect()
}

fn workspace() -> (TempDir, PathBuf) {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("ws");
    write(&root, "a.md", b"Hello World\nsecond line\nhello again\n");
    write(&root, "sub/b.txt", b"nothing\r\nHELLO there\r\n");
    write(&root, ".gitignore", b"ignored/\n*.log\n");
    write(&root, "ignored/c.md", b"hello\n");
    write(&root, "x.log", b"hello\n");
    write(&root, ".hidden/h.md", b"hello\n");
    write(&root, ".git/config", b"hello\n");
    write(&root, "bin.dat", b"hello\0world\n");
    write(&root, "empty.txt", b"");
    (tmp, root)
}

#[test]
fn literal_search_is_case_insensitive_and_honors_ignore_rules() {
    let (_tmp, root) = workspace();
    let (found, summary) = search(&root, &SearchOptions::literal("hello"));

    assert_eq!(
        found.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        [root.join("a.md"), root.join("sub").join("b.txt")]
    );
    assert_eq!(lines(&found[0]), [1, 3]);
    assert_eq!(lines(&found[1]), [2]);

    let first = &found[0].matches[0];
    assert_eq!((first.column, first.preview.as_str()), (0, "Hello World"));
    assert_eq!(first.ranges, [Range { start: 0, end: 5 }]);
    // CRLF terminators never show up in the preview.
    assert_eq!(found[1].matches[0].preview, "HELLO there");

    assert_eq!(
        summary,
        SearchSummary {
            files_searched: 3,
            files_matched: 2,
            matches: 3,
            binary_skipped: 1,
            placeholders_skipped: 0,
            unreadable: 0,
            truncated: false,
            cancelled: false,
        }
    );
}

#[test]
fn hidden_items_are_searched_on_request_but_never_git_internals() {
    let (_tmp, root) = workspace();
    let options = SearchOptions {
        show_hidden: true,
        ..SearchOptions::literal("hello")
    };
    let (found, _) = search(&root, &options);
    let paths: Vec<PathBuf> = found.into_iter().map(|f| f.path).collect();
    assert!(paths.contains(&root.join(".hidden").join("h.md")));
    assert!(!paths.iter().any(|p| p.starts_with(root.join(".git"))));
    assert!(!paths.iter().any(|p| p.starts_with(root.join("ignored"))));
}

#[test]
fn case_whole_word_and_regex_options() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "words.txt", b"shell\nhello\nhellos\nHello World\n");

    let base = SearchOptions::literal("hello");
    let lines_of = |options: &SearchOptions| {
        let (found, _) = search(root, options);
        found.first().map(lines).unwrap_or_default()
    };
    assert_eq!(lines_of(&base), [2, 3, 4]);
    assert_eq!(
        lines_of(&SearchOptions {
            case_sensitive: true,
            ..base.clone()
        }),
        [2, 3]
    );
    assert_eq!(
        lines_of(&SearchOptions {
            whole_word: true,
            ..base.clone()
        }),
        [2, 4]
    );
    assert!(
        lines_of(&SearchOptions::literal("hell\\w")).is_empty(),
        "literal text, not a regex"
    );

    let regex = SearchOptions {
        regex: true,
        ..SearchOptions::literal("he(l+)o\\s+w")
    };
    let (found, _) = search(root, &regex);
    assert_eq!(lines(&found[0]), [4]);
    assert_eq!(found[0].matches[0].ranges, [Range { start: 0, end: 7 }]);
}

#[test]
fn invalid_and_empty_patterns_are_rejected_before_searching() {
    let tmp = tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let invalid = SearchOptions {
        regex: true,
        ..SearchOptions::literal("(")
    };
    let error = try_search(tmp.path(), &invalid, &|_| false, &cancel).unwrap_err();
    assert!(matches!(error, SearchError::InvalidPattern(_)));
    assert!(error.to_string().starts_with("invalid search pattern"));

    let error =
        try_search(tmp.path(), &SearchOptions::literal(""), &|_| false, &cancel).unwrap_err();
    assert!(matches!(error, SearchError::EmptyPattern));
    assert!(error.to_string().contains("empty"));
}

#[test]
fn columns_are_utf8_byte_offsets_and_every_match_has_a_range() {
    let tmp = tempdir().unwrap();
    write(
        tmp.path(),
        "u.md",
        "ünal ünal Hello\nhello hello\n".as_bytes(),
    );
    let (found, _) = search(tmp.path(), &SearchOptions::literal("hello"));
    let matches = &found[0].matches;
    assert_eq!(
        (matches[0].column, matches[0].ranges.clone()),
        (12, vec![Range { start: 12, end: 17 }])
    );
    assert_eq!(matches[0].preview, "ünal ünal Hello");
    assert_eq!(
        (matches[1].column, matches[1].ranges.clone()),
        (0, vec![0..5, 6..11])
    );
}

#[test]
fn long_lines_are_cut_on_a_character_boundary() {
    let tmp = tempdir().unwrap();
    let ascii = format!("{}needle\n", "x".repeat(500));
    // 1 + 2 * 250 bytes before the match: byte 400 falls inside a character.
    let wide = format!("a{}needle\n", "ü".repeat(250));
    write(tmp.path(), "ascii.txt", ascii.as_bytes());
    write(tmp.path(), "wide.txt", wide.as_bytes());
    let (found, _) = search(tmp.path(), &SearchOptions::literal("needle"));

    let ascii_match = &found[0].matches[0];
    assert_eq!(ascii_match.preview.len(), MAX_PREVIEW_BYTES);
    // The match lies beyond the preview, so it has no range there.
    assert!(ascii_match.ranges.is_empty());
    assert_eq!(ascii_match.column, 0);

    let wide_match = &found[1].matches[0];
    assert_eq!(wide_match.preview.len(), MAX_PREVIEW_BYTES - 1);
    assert!(wide_match.preview.starts_with("aü"));
}

#[test]
fn utf8_utf16_and_legacy_code_pages_are_decoded_before_matching() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let names = [
        "uchardet-tr-utf-8.txt",
        "derived-tr-utf-8-bom.txt",
        "derived-tr-utf-16le-bom.txt",
        "derived-tr-utf-16be-bom.txt",
        "derived-tr-windows-1254.txt",
    ];
    for name in names {
        write(root, name, &fixture(name));
    }
    let (found, summary) = search(root, &SearchOptions::literal("türkiye"));
    assert_eq!(summary.files_matched, names.len(), "{summary:?}");
    assert_eq!(summary.binary_skipped, 0, "UTF-16 with a BOM is text");
    for file in &found {
        let first = &file.matches[0];
        let name = file.path.file_name().unwrap().to_string_lossy();
        assert_eq!(first.line, 1, "{name}");
        assert_eq!(first.column, 0, "{name}");
        assert_eq!(first.ranges[0], 0..8, "{name}");
        assert!(
            first.preview.starts_with("Türkiye, resmî ad"),
            "{name}: {}",
            first.preview
        );
    }
}

#[test]
fn shift_jis_text_is_found_through_its_decoded_form() {
    let tmp = tempdir().unwrap();
    let bytes = fixture("derived-ja-shift_jis.txt");
    let (decoded, _, had_errors) = encoding_rs::SHIFT_JIS.decode(&bytes);
    assert!(!had_errors);
    let first_line = decoded.lines().find(|l| l.chars().count() >= 3).unwrap();
    let needle: String = first_line.chars().take(3).collect();
    write(tmp.path(), "sjis.txt", &bytes);

    let (found, _) = search(tmp.path(), &SearchOptions::literal(&needle));
    assert_eq!(found.len(), 1, "needle {needle:?}");
    assert_eq!(found[0].matches[0].column, 0);
    assert!(found[0].matches[0].preview.starts_with(&needle));
}

#[test]
fn bytes_that_are_not_valid_utf8_fall_back_to_a_guessed_encoding() {
    let tmp = tempdir().unwrap();
    write(tmp.path(), "latin.txt", b"caf\xE9 hello\n");
    let (found, summary) = search(tmp.path(), &SearchOptions::literal("hello"));
    assert_eq!(summary.files_matched, 1);
    assert_eq!(lines(&found[0]), [1]);
    assert!(found[0].matches[0].preview.ends_with(" hello"));
}

#[test]
fn match_budget_truncates_the_search() {
    let tmp = tempdir().unwrap();
    write(tmp.path(), "many.txt", b"hit\nhit\nhit\nhit\nhit\n");
    write(tmp.path(), "more.txt", b"hit\n");
    let options = SearchOptions {
        max_matches: 3,
        ..SearchOptions::literal("hit")
    };
    let (found, summary) = search(tmp.path(), &options);
    assert!(summary.truncated);
    assert_eq!(summary.matches, 3);
    assert_eq!(found.iter().map(|f| f.matches.len()).sum::<usize>(), 3);
}

#[test]
fn big_files_and_placeholders_are_not_read() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "small.txt", b"hit\n");
    write(
        root,
        "big.txt",
        format!("hit\n{}", "x".repeat(200)).as_bytes(),
    );
    let cloud = write(root, "cloud.txt", b"hit\n");

    let options = SearchOptions {
        max_file_bytes: 100,
        ..SearchOptions::literal("hit")
    };
    let (found, summary) = search_with(root, &options, &|path| path == cloud);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, root.join("small.txt"));
    assert_eq!(summary.placeholders_skipped, 1);
    assert_eq!(summary.files_searched, 1);
}

#[test]
fn files_that_cannot_be_opened_are_counted_not_fatal() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "ok.txt", b"hit\n");
    let locked = write(root, "locked.txt", b"hit\n");
    let _held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&locked)
        .unwrap();
    let (found, summary) = search(root, &SearchOptions::literal("hit"));
    assert_eq!(found.len(), 1);
    assert_eq!(summary.unreadable, 1);
    assert_eq!(summary.files_matched, 1);
}

/// Locks `len` bytes at `offset` of `file` against every other handle (reads of the range fail
/// with `ERROR_LOCK_VIOLATION`) until `file` is closed.
fn lock_range(file: &std::fs::File, offset: u64, len: u32) {
    let mut overlapped = OVERLAPPED::default();
    // SAFETY: the handle belongs to `file`, which outlives the call; `overlapped` is a valid
    // `OVERLAPPED` that only carries the offset of this synchronous call.
    unsafe {
        overlapped.Anonymous.Anonymous.Offset = offset as u32;
        overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
        LockFileEx(
            HANDLE(file.as_raw_handle()),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            None,
            len,
            0,
            &mut overlapped,
        )
    }
    .unwrap();
}

#[test]
fn files_that_fail_while_being_read_are_counted_not_fatal() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    write(root, "ok.txt", b"hit\n");
    let content = format!("hit\n{}", "x".repeat(49_996));
    // One file fails inside the 32 KiB encoding sample, the other only after it, while the
    // searcher streams the rest of the file.
    let early = write(root, "early.txt", content.as_bytes());
    let late = write(root, "late.txt", content.as_bytes());
    let mut held = Vec::new();
    for (path, offset) in [(&early, 0), (&late, 40_000)] {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        lock_range(&file, offset, 1_000);
        held.push(file);
    }
    let (found, summary) = search(root, &SearchOptions::literal("hit"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, root.join("ok.txt"));
    assert_eq!(summary.unreadable, 2);
    assert_eq!(summary.files_matched, 1);
    drop(held);
    // Without the locks both files are searched normally.
    let (found, summary) = search(root, &SearchOptions::literal("hit"));
    assert_eq!(found.len(), 3);
    assert_eq!(summary.unreadable, 0);
}

#[test]
fn cancelling_stops_at_the_next_file() {
    let tmp = tempdir().unwrap();
    for name in ["a.txt", "b.txt", "c.txt"] {
        write(tmp.path(), name, b"hit\n");
    }

    // Cancelled before the first file.
    let cancel = AtomicBool::new(true);
    let (found, summary) = try_search(
        tmp.path(),
        &SearchOptions::literal("hit"),
        &|_| false,
        &cancel,
    )
    .unwrap();
    assert!(summary.cancelled);
    assert_eq!((found.len(), summary.files_searched), (0, 0));

    // Cancelled from the result callback after the first file.
    let cancel = AtomicBool::new(false);
    let mut reported = 0;
    let summary = search_folder(
        tmp.path(),
        &SearchOptions::literal("hit"),
        &|_| false,
        &cancel,
        |_| {
            reported += 1;
            cancel.store(true, Ordering::Relaxed);
        },
    )
    .unwrap();
    assert!(summary.cancelled);
    assert_eq!(reported, 1);
}

#[test]
fn nothing_matching_is_an_empty_successful_search() {
    let (_tmp, root) = workspace();
    let (found, summary) = search(&root, &SearchOptions::literal("zzzz-not-there"));
    assert!(found.is_empty());
    assert_eq!(summary.matches, 0);
    assert_eq!(summary.files_matched, 0);
    assert!(!summary.truncated && !summary.cancelled);
}
