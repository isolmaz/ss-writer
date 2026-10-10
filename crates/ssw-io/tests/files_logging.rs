use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ssw_io::fs::FsError;
use ssw_io::logging::{Level, LogOptions, Logger, format_timestamp};

const MB: u64 = 1024 * 1024;

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn timestamps_are_iso_8601_in_utc() {
    let table = [
        (0, 0, "1970-01-01T00:00:00.000Z"),
        (86_399, 0, "1970-01-01T23:59:59.000Z"),
        (86_400, 5, "1970-01-02T00:00:00.005Z"),
        (946_684_799, 999, "1999-12-31T23:59:59.999Z"),
        (951_782_399, 0, "2000-02-28T23:59:59.000Z"),
        (951_782_400, 0, "2000-02-29T00:00:00.000Z"),
        (951_868_800, 0, "2000-03-01T00:00:00.000Z"),
        (1_000_000_000, 0, "2001-09-09T01:46:40.000Z"),
        (1_234_567_890, 0, "2009-02-13T23:31:30.000Z"),
        (1_709_210_096, 789, "2024-02-29T12:34:56.789Z"),
        (2_147_483_647, 0, "2038-01-19T03:14:07.000Z"),
        (4_102_444_800, 0, "2100-01-01T00:00:00.000Z"),
        (4_107_542_400, 0, "2100-03-01T00:00:00.000Z"),
    ];
    for (seconds, millis, expected) in table {
        let time = UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000);
        assert_eq!(format_timestamp(time), expected);
    }
    // Times before 1970 collapse to the epoch instead of failing.
    assert_eq!(
        format_timestamp(UNIX_EPOCH - Duration::from_secs(1)),
        "1970-01-01T00:00:00.000Z"
    );
}

#[test]
fn levels_are_ordered_and_tagged() {
    assert!(Level::Error < Level::Warn);
    assert!(Level::Warn < Level::Info);
    assert!(Level::Info < Level::Debug);
    assert_eq!(Level::Error.tag(), "ERROR");
    assert_eq!(Level::Warn.tag(), "WARN ");
    assert_eq!(Level::Info.tag(), "INFO ");
    assert_eq!(Level::Debug.tag(), "DEBUG");
}

#[test]
fn the_defaults_are_five_files_of_one_megabyte_at_info() {
    let options = LogOptions::default();
    assert_eq!(options.max_bytes, MB);
    assert_eq!(options.files, 5);
    assert_eq!(options.level, Level::Info);
}

#[test]
fn records_have_a_timestamp_level_target_and_single_line_message() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("logs").join("nested");
    let logger = Logger::new(&logs, LogOptions::default()).unwrap();
    assert_eq!(logger.paths(), [logs.join("ssw.log")]);

    let before = format_timestamp(SystemTime::now());
    logger.log(Level::Info, "app", "started");
    logger.log(Level::Error, "io::save", "disk\nfull\r\nnow");
    logger.log(Level::Warn, "x", "");
    let after = format_timestamp(SystemTime::now());

    let lines = read_lines(&logs.join("ssw.log"));
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(&lines[0][24..], " INFO  app: started");
    assert_eq!(&lines[1][24..], " ERROR io::save: disk\\nfull\r\\nnow");
    assert_eq!(&lines[2][24..], " WARN  x: ");
    for line in &lines {
        let stamp = &line[..24];
        assert!(
            stamp >= before.as_str() && stamp <= after.as_str(),
            "{stamp} not in {before}..{after}"
        );
        assert_eq!(stamp.as_bytes()[10], b'T');
        assert!(stamp.ends_with('Z'));
    }
}

#[test]
fn only_records_up_to_the_configured_level_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let logger = Logger::new(dir.path(), LogOptions::default()).unwrap();
    let log_all = |logger: &Logger| {
        logger.log(Level::Error, "t", "error");
        logger.log(Level::Warn, "t", "warn");
        logger.log(Level::Info, "t", "info");
        logger.log(Level::Debug, "t", "debug");
    };
    let messages = || -> Vec<String> {
        read_lines(&dir.path().join("ssw.log"))
            .into_iter()
            .map(|line| line.rsplit(": ").next().unwrap().to_owned())
            .collect()
    };

    log_all(&logger);
    assert_eq!(messages(), ["error", "warn", "info"]);

    logger.set_level(Level::Debug);
    log_all(&logger);
    assert_eq!(messages().len(), 3 + 4);
    assert_eq!(messages().last().unwrap(), "debug");

    logger.set_level(Level::Error);
    log_all(&logger);
    assert_eq!(messages().len(), 3 + 4 + 1);
    assert_eq!(messages().last().unwrap(), "error");

    let quiet = Logger::new(
        &dir.path().join("quiet"),
        LogOptions {
            level: Level::Warn,
            ..LogOptions::default()
        },
    )
    .unwrap();
    log_all(&quiet);
    assert_eq!(
        read_lines(&dir.path().join("quiet").join("ssw.log")).len(),
        2
    );
}

#[test]
fn rotation_keeps_five_files_of_at_most_one_megabyte() {
    let dir = tempfile::tempdir().unwrap();
    let logger = Logger::new(dir.path(), LogOptions::default()).unwrap();
    let filler = "x".repeat(1000);
    let total = 7000;
    for sequence in 0..total {
        logger.log(
            Level::Info,
            "rotation",
            &format!("seq={sequence:06} {filler}"),
        );
    }

    let paths = logger.paths();
    let expected: Vec<PathBuf> = [
        "ssw.log",
        "ssw.1.log",
        "ssw.2.log",
        "ssw.3.log",
        "ssw.4.log",
    ]
    .iter()
    .map(|name| dir.path().join(name))
    .collect();
    assert_eq!(paths, expected);
    assert_eq!(
        file_names(dir.path()),
        [
            "ssw.1.log",
            "ssw.2.log",
            "ssw.3.log",
            "ssw.4.log",
            "ssw.log"
        ]
    );

    let sequences = |path: &Path| -> Vec<u32> {
        read_lines(path)
            .iter()
            .map(|line| line.split("seq=").nth(1).unwrap()[..6].parse().unwrap())
            .collect()
    };
    let mut previous_first = u32::MAX;
    for (index, path) in paths.iter().enumerate() {
        let length = size(path);
        assert!(length <= MB, "{path:?} is {length} bytes");
        if index > 0 {
            assert!(
                length > MB - 2000,
                "{path:?} was rotated while far from full: {length}"
            );
        }
        let numbers = sequences(path);
        assert!(
            numbers.windows(2).all(|pair| pair[1] == pair[0] + 1),
            "{path:?} is contiguous"
        );
        assert!(
            *numbers.last().unwrap() < previous_first,
            "newer files hold newer records"
        );
        previous_first = numbers[0];
    }
    let newest = sequences(&paths[0]);
    assert_eq!(
        *newest.last().unwrap(),
        total - 1,
        "the newest record is in ssw.log"
    );
    let oldest = sequences(&paths[4]);
    assert!(oldest[0] > 0, "the oldest records were dropped");
}

#[test]
fn small_limits_rotate_through_the_configured_number_of_files() {
    let dir = tempfile::tempdir().unwrap();
    let options = LogOptions {
        max_bytes: 200,
        files: 3,
        level: Level::Info,
    };
    let logger = Logger::new(dir.path(), options).unwrap();
    for sequence in 0..30 {
        logger.log(Level::Info, "t", &format!("record {sequence:02}"));
    }
    assert_eq!(
        file_names(dir.path()),
        ["ssw.1.log", "ssw.2.log", "ssw.log"]
    );
    assert_eq!(logger.paths().len(), 3);
    let mut seen = Vec::new();
    for name in ["ssw.2.log", "ssw.1.log", "ssw.log"] {
        let path = dir.path().join(name);
        assert!(size(&path) <= 200, "{name}");
        seen.extend(read_lines(&path));
    }
    assert!(seen.last().unwrap().ends_with("record 29"));
    assert!(
        !seen.iter().any(|line| line.ends_with("record 00")),
        "old records are gone"
    );
    // Records stay in order across the files.
    let numbers: Vec<u32> = seen
        .iter()
        .map(|line| line.rsplit("record ").next().unwrap().parse().unwrap())
        .collect();
    assert!(numbers.windows(2).all(|pair| pair[1] == pair[0] + 1));
}

#[test]
fn an_oversized_record_is_written_whole_and_the_next_one_rotates() {
    let dir = tempfile::tempdir().unwrap();
    let options = LogOptions {
        max_bytes: 100,
        files: 2,
        level: Level::Info,
    };
    let logger = Logger::new(dir.path(), options).unwrap();
    let big = "b".repeat(300);
    logger.log(Level::Info, "t", &big);
    assert_eq!(
        file_names(dir.path()),
        ["ssw.log"],
        "an empty file takes any record"
    );
    assert!(size(&dir.path().join("ssw.log")) > 300);

    logger.log(Level::Info, "t", "small");
    assert_eq!(file_names(dir.path()), ["ssw.1.log", "ssw.log"]);
    assert!(read_lines(&dir.path().join("ssw.1.log"))[0].ends_with(&big));
    assert!(read_lines(&dir.path().join("ssw.log"))[0].ends_with("small"));
}

#[test]
fn an_existing_log_is_continued_and_counted_towards_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ssw.log"), "x".repeat(150)).unwrap();
    let options = LogOptions {
        max_bytes: 200,
        files: 3,
        level: Level::Info,
    };
    let logger = Logger::new(dir.path(), options).unwrap();
    logger.log(
        Level::Info,
        "t",
        "a record that does not fit the remaining fifty bytes",
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("ssw.1.log")).unwrap(),
        "x".repeat(150)
    );
    assert_eq!(read_lines(&dir.path().join("ssw.log")).len(), 1);

    // Opening again appends to what is there.
    drop(logger);
    let again = Logger::new(dir.path(), LogOptions::default()).unwrap();
    again.log(Level::Info, "t", "second run");
    let lines = read_lines(&dir.path().join("ssw.log"));
    assert_eq!(lines.len(), 2);
    assert!(lines[1].ends_with("second run"));
}

#[test]
fn many_threads_never_interleave_inside_a_record() {
    let dir = tempfile::tempdir().unwrap();
    let logger = Arc::new(Logger::new(dir.path(), LogOptions::default()).unwrap());
    let handles: Vec<_> = (0..4)
        .map(|thread| {
            let logger = Arc::clone(&logger);
            std::thread::spawn(move || {
                for record in 0..300 {
                    logger.log(
                        Level::Info,
                        "threads",
                        &format!("thread={thread} record={record:03} {} end", "y".repeat(200)),
                    );
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    let lines = read_lines(&dir.path().join("ssw.log"));
    assert_eq!(lines.len(), 1200);
    assert!(
        lines
            .iter()
            .all(|line| line.ends_with(" end") && line.contains(" INFO  threads: thread="))
    );
    for thread in 0..4 {
        let own: Vec<&String> = lines
            .iter()
            .filter(|line| line.contains(&format!("thread={thread} ")))
            .collect();
        assert_eq!(own.len(), 300);
    }
}

#[test]
fn opening_the_log_reports_folder_and_file_problems() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("not-a-folder");
    std::fs::write(&blocker, "x").unwrap();
    assert!(matches!(
        Logger::new(&blocker, LogOptions::default()),
        Err(FsError::AlreadyExists(_) | FsError::Os { .. })
    ));
    // `ssw.log` cannot be a folder.
    let clash = dir.path().join("clash");
    std::fs::create_dir_all(clash.join("ssw.log")).unwrap();
    assert!(matches!(
        Logger::new(&clash, LogOptions::default()),
        Err(FsError::AccessDenied(_))
    ));
}

#[test]
fn a_single_kept_file_starts_over_in_place_without_losing_records() {
    // `files: 0` still keeps the current file.
    for files in [1usize, 0] {
        let dir = tempfile::tempdir().unwrap();
        let options = LogOptions {
            max_bytes: 200,
            files,
            level: Level::Info,
        };
        let logger = Logger::new(dir.path(), options).unwrap();
        for sequence in 0..30 {
            logger.log(Level::Info, "t", &format!("record {sequence:02}"));
        }
        assert_eq!(file_names(dir.path()), ["ssw.log"], "files = {files}");
        assert_eq!(logger.paths(), [dir.path().join("ssw.log")]);
        let path = dir.path().join("ssw.log");
        assert!(size(&path) <= 200);
        let numbers: Vec<u32> = read_lines(&path)
            .iter()
            .map(|line| line.rsplit("record ").next().unwrap().parse().unwrap())
            .collect();
        assert!((1..=4).contains(&numbers.len()), "{numbers:?}");
        assert_eq!(*numbers.last().unwrap(), 29, "the newest record is kept");
        assert!(numbers.windows(2).all(|pair| pair[1] == pair[0] + 1));
        assert!(numbers[0] > 0, "older records were dropped");
    }
}
