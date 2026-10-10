use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use ssw_io::fs::{FaultFs, FileSystem, FsError, FsFailure, FsOp, RealFs};
use ssw_io::save::{SaveStamp, ThreadSleeper, save_atomic};
use ssw_io::watch::{
    ChangeSource, Coalescer, DEBOUNCE, DocChange, ExternalChanges, FsEvent, FsEventKind, MAX_DELAY,
    Poll, RawAction, RawChange, Watcher, parse_notifications, path_key, pump,
};

const ROOT: &str = "C:\\watched";

fn at(name: &str) -> PathBuf {
    Path::new(ROOT).join(name)
}

fn raw(action: RawAction, name: &str) -> RawChange {
    RawChange {
        action,
        path: at(name),
    }
}

fn ev(name: &str, kind: FsEventKind) -> FsEvent {
    FsEvent {
        path: at(name),
        kind,
    }
}

fn renamed(from: &str) -> FsEventKind {
    FsEventKind::Renamed { from: at(from) }
}

fn coalesce(changes: Vec<RawChange>) -> Vec<FsEvent> {
    let mut coalescer = Coalescer::new();
    let now = Instant::now();
    for change in changes {
        coalescer.push(change, now);
    }
    coalescer.take_all()
}

#[test]
fn raw_changes_merge_into_one_event_per_path() {
    use FsEventKind::{Created, Modified, Removed, Rescan};
    use RawAction::{Added, Modified as Mod, Overflow, Removed as Rem, RenamedNew, RenamedOld};
    let table: Vec<(&str, Vec<RawChange>, Vec<FsEvent>)> = vec![
        (
            "added then modified is one create",
            vec![raw(Added, "a"), raw(Mod, "a"), raw(Mod, "a")],
            vec![ev("a", Created)],
        ),
        (
            "added twice stays one create",
            vec![raw(Added, "a"), raw(Added, "a")],
            vec![ev("a", Created)],
        ),
        (
            "created and deleted inside one window is nothing",
            vec![raw(Added, "a"), raw(Mod, "a"), raw(Rem, "a")],
            vec![],
        ),
        (
            "removed then added is a modification",
            vec![raw(Rem, "a"), raw(Added, "a")],
            vec![ev("a", Modified)],
        ),
        (
            "removed, re-added and removed again is a removal",
            vec![raw(Rem, "a"), raw(Added, "a"), raw(Rem, "a")],
            vec![ev("a", Removed)],
        ),
        (
            "modified then removed is a removal",
            vec![raw(Mod, "a"), raw(Rem, "a")],
            vec![ev("a", Removed)],
        ),
        (
            "repeated modifications are one",
            vec![raw(Mod, "a"), raw(Mod, "a")],
            vec![ev("a", Modified)],
        ),
        (
            "removal wins over a later modification and repeats",
            vec![raw(Rem, "a"), raw(Mod, "a"), raw(Rem, "a")],
            vec![ev("a", Removed)],
        ),
        (
            "an old/new pair is a rename",
            vec![raw(RenamedOld, "a"), raw(RenamedNew, "b")],
            vec![ev("b", renamed("a"))],
        ),
        (
            "modifications after a rename keep the rename",
            vec![
                raw(RenamedOld, "a"),
                raw(RenamedNew, "b"),
                raw(Mod, "b"),
                raw(Added, "b"),
            ],
            vec![ev("b", renamed("a"))],
        ),
        (
            "a file created and renamed in one window appears under the new name",
            vec![raw(Added, "a"), raw(RenamedOld, "a"), raw(RenamedNew, "b")],
            vec![ev("b", Created)],
        ),
        (
            "renaming twice in one window",
            vec![
                raw(Added, "a"),
                raw(RenamedOld, "a"),
                raw(RenamedNew, "b"),
                raw(RenamedOld, "b"),
                raw(RenamedNew, "c"),
            ],
            vec![ev("c", Created)],
        ),
        (
            "renamed then removed removes both names",
            vec![raw(RenamedOld, "a"), raw(RenamedNew, "b"), raw(Rem, "b")],
            vec![ev("b", Removed), ev("a", Removed)],
        ),
        (
            "the old name can be reused after a rename",
            vec![raw(RenamedOld, "a"), raw(RenamedNew, "b"), raw(Added, "a")],
            vec![ev("b", renamed("a")), ev("a", Created)],
        ),
        (
            "an old name without a new name left the tree",
            vec![raw(RenamedOld, "a")],
            vec![ev("a", Removed)],
        ),
        (
            "a second old name means the first left the tree",
            vec![
                raw(RenamedOld, "a"),
                raw(RenamedOld, "b"),
                raw(RenamedNew, "c"),
            ],
            vec![ev("a", Removed), ev("c", renamed("b"))],
        ),
        (
            "a new name without an old name came from outside",
            vec![raw(RenamedNew, "b")],
            vec![ev("b", Created)],
        ),
        (
            "an overflow asks for a rescan of the root",
            vec![RawChange {
                action: Overflow,
                path: PathBuf::from(ROOT),
            }],
            vec![FsEvent {
                path: PathBuf::from(ROOT),
                kind: Rescan,
            }],
        ),
        (
            "a rescan absorbs later changes of the same path",
            vec![
                RawChange {
                    action: Overflow,
                    path: PathBuf::from(ROOT),
                },
                RawChange {
                    action: Mod,
                    path: PathBuf::from(ROOT),
                },
                raw(Mod, "x"),
            ],
            vec![
                FsEvent {
                    path: PathBuf::from(ROOT),
                    kind: Rescan,
                },
                ev("x", Modified),
            ],
        ),
        (
            "paths compare case-insensitively and keep the first spelling",
            vec![raw(Added, "A.md"), raw(Mod, "a.MD")],
            vec![ev("A.md", Created)],
        ),
        (
            "events keep the order of first appearance",
            vec![raw(Mod, "c"), raw(Mod, "a"), raw(Mod, "b"), raw(Mod, "a")],
            vec![ev("c", Modified), ev("a", Modified), ev("b", Modified)],
        ),
    ];
    for (name, changes, expected) in table {
        assert_eq!(coalesce(changes), expected, "{name}");
    }
}

#[test]
fn the_debounce_window_follows_the_last_change() {
    let mut coalescer = Coalescer::new();
    let start = Instant::now();
    let ms = Duration::from_millis;
    assert_eq!(coalescer.timeout(start), None);
    assert!(coalescer.take_ready(start).is_empty());

    coalescer.push(raw(RawAction::Modified, "a"), start);
    assert_eq!(DEBOUNCE, ms(50));
    assert_eq!(coalescer.timeout(start), Some(DEBOUNCE));
    assert_eq!(coalescer.timeout(start + ms(20)), Some(ms(30)));
    assert_eq!(coalescer.timeout(start + ms(80)), Some(Duration::ZERO));
    assert!(coalescer.take_ready(start + ms(49)).is_empty());

    // A later change pushes the deadline out.
    coalescer.push(raw(RawAction::Modified, "b"), start + ms(40));
    assert!(coalescer.take_ready(start + ms(89)).is_empty());
    let ready = coalescer.take_ready(start + ms(90));
    assert_eq!(
        ready,
        [
            ev("a", FsEventKind::Modified),
            ev("b", FsEventKind::Modified)
        ]
    );
    assert_eq!(coalescer.timeout(start + ms(90)), None);
    assert!(coalescer.take_ready(start + ms(500)).is_empty());
}

#[test]
fn a_steady_stream_cannot_delay_delivery_beyond_the_maximum() {
    let mut coalescer = Coalescer::new();
    let start = Instant::now();
    let ms = Duration::from_millis;
    assert_eq!(MAX_DELAY, ms(500));
    for step in 0..=12 {
        coalescer.push(raw(RawAction::Modified, "a"), start + ms(40 * step));
    }
    // Last change at 480 ms: debounce alone would wait until 530 ms, the cap says 500 ms.
    assert_eq!(coalescer.timeout(start + ms(480)), Some(ms(20)));
    assert!(coalescer.take_ready(start + ms(499)).is_empty());
    assert_eq!(
        coalescer.take_ready(start + ms(500)),
        [ev("a", FsEventKind::Modified)]
    );
}

#[test]
fn take_all_releases_immediately_and_resets_the_window() {
    let mut coalescer = Coalescer::new();
    let start = Instant::now();
    coalescer.push(raw(RawAction::Added, "a"), start);
    assert_eq!(coalescer.take_all(), [ev("a", FsEventKind::Created)]);
    assert_eq!(coalescer.timeout(start), None);
    assert!(coalescer.take_all().is_empty());
    // The next change starts a fresh window instead of inheriting the old one.
    let later = start + Duration::from_secs(10);
    coalescer.push(raw(RawAction::Modified, "b"), later);
    assert_eq!(coalescer.timeout(later), Some(DEBOUNCE));
}

#[test]
fn path_keys_ignore_case_separators_and_trailing_slashes() {
    assert_eq!(path_key(Path::new("C:/Users/Alice/")), "c:\\users\\alice");
    assert_eq!(
        path_key(Path::new("C:\\Users\\ALICE\\\\")),
        "c:\\users\\alice"
    );
    assert_eq!(path_key(Path::new("c:\\users\\alice")), "c:\\users\\alice");
    assert_eq!(path_key(Path::new("C:\\")), "c:");
    assert_ne!(path_key(Path::new("C:\\a")), path_key(Path::new("C:\\b")));
}

/// Builds a `FILE_NOTIFY_INFORMATION` chain: `NextEntryOffset`, `Action`, `FileNameLength`, name.
fn chain(entries: &[(u32, &str)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, (action, name)) in entries.iter().enumerate() {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let length = 12 + units.len();
        let padded = length.div_ceil(4) * 4;
        let next = if index + 1 == entries.len() {
            0
        } else {
            u32::try_from(padded).unwrap()
        };
        out.extend(next.to_le_bytes());
        out.extend(action.to_le_bytes());
        out.extend(u32::try_from(units.len()).unwrap().to_le_bytes());
        out.extend(units);
        out.resize(out.len() + padded - length, 0);
    }
    out
}

#[test]
fn notification_buffers_are_parsed_into_raw_changes() {
    let root = Path::new(ROOT);
    let buffer = chain(&[
        (1, "new.md"),
        (2, "old.md"),
        (3, "sub\\mod.md"),
        (4, "a.md"),
        (5, "İstanbul ışık ✓.md"),
    ]);
    let parsed = parse_notifications(&buffer, buffer.len(), root);
    let expected = vec![
        RawChange {
            action: RawAction::Added,
            path: root.join("new.md"),
        },
        RawChange {
            action: RawAction::Removed,
            path: root.join("old.md"),
        },
        RawChange {
            action: RawAction::Modified,
            path: root.join("sub\\mod.md"),
        },
        RawChange {
            action: RawAction::RenamedOld,
            path: root.join("a.md"),
        },
        RawChange {
            action: RawAction::RenamedNew,
            path: root.join("İstanbul ışık ✓.md"),
        },
    ];
    assert_eq!(parsed, expected);

    // The OS reports how many bytes are valid; a buffer larger than that is fine, and a count
    // larger than the buffer is clamped.
    let mut padded = buffer.clone();
    padded.resize(buffer.len() + 100, 0xAA);
    assert_eq!(parse_notifications(&padded, buffer.len(), root), expected);
    assert_eq!(
        parse_notifications(&buffer, buffer.len() + 100, root),
        expected
    );
}

#[test]
fn zero_transferred_bytes_mean_the_kernel_buffer_overflowed() {
    let root = Path::new(ROOT);
    let garbage = [0xFFu8; 64];
    assert_eq!(
        parse_notifications(&garbage, 0, root),
        [RawChange {
            action: RawAction::Overflow,
            path: root.to_path_buf()
        }]
    );
}

#[test]
fn damaged_notification_buffers_keep_what_was_readable() {
    let root = Path::new(ROOT);
    let ok = RawChange {
        action: RawAction::Added,
        path: root.join("ok.md"),
    };

    // Unknown actions are skipped, later records still count.
    let skipped = chain(&[(9, "skip.md"), (1, "ok.md")]);
    assert_eq!(
        parse_notifications(&skipped, skipped.len(), root),
        std::slice::from_ref(&ok)
    );

    // Fewer valid bytes than the chain claims: the second record is cut off.
    let two = chain(&[(1, "ok.md"), (2, "gone.md")]);
    let first_len = u32::from_le_bytes([two[0], two[1], two[2], two[3]]) as usize;
    assert_eq!(
        parse_notifications(&two, first_len, root),
        std::slice::from_ref(&ok)
    );

    // A header that is cut short.
    assert!(parse_notifications(&two, 8, root).is_empty());
    assert!(parse_notifications(&two, 11, root).is_empty());

    // A name that runs past the valid bytes.
    let mut long_name = chain(&[(1, "ok.md")]);
    long_name[8..12].copy_from_slice(&1000u32.to_le_bytes());
    assert!(parse_notifications(&long_name, long_name.len(), root).is_empty());

    // A next-entry offset that points outside the buffer ends the chain after the first record.
    let mut wild = chain(&[(1, "ok.md")]);
    wild[0..4].copy_from_slice(&4096u32.to_le_bytes());
    assert_eq!(
        parse_notifications(&wild, wild.len(), root),
        std::slice::from_ref(&ok)
    );

    // An odd trailing byte of the name is dropped.
    let mut odd = chain(&[(1, "ab")]);
    odd[8..12].copy_from_slice(&3u32.to_le_bytes());
    assert_eq!(
        parse_notifications(&odd, odd.len(), root),
        [RawChange {
            action: RawAction::Added,
            path: root.join("a")
        }]
    );
}

struct Scripted {
    items: VecDeque<(u64, Poll)>,
    clock: Rc<Cell<Instant>>,
    timeouts: Vec<Option<Duration>>,
}

impl ChangeSource for Scripted {
    fn poll(&mut self, timeout: Option<Duration>) -> Poll {
        self.timeouts.push(timeout);
        let (advance, polled) = self.items.pop_front().expect("the script ran out");
        self.clock
            .set(self.clock.get() + Duration::from_millis(advance));
        polled
    }
}

type Batches = Vec<(Duration, Vec<FsEvent>)>;

/// Runs the pump over `items` (clock advance in ms before each poll result) and returns the
/// delivered batches with their delivery time since the start, plus the timeouts it asked for.
fn run_pump(items: Vec<(u64, Poll)>) -> (Batches, Vec<Option<Duration>>) {
    let start = Instant::now();
    let clock = Rc::new(Cell::new(start));
    let mut source = Scripted {
        items: items.into(),
        clock: Rc::clone(&clock),
        timeouts: Vec::new(),
    };
    let mut batches = Vec::new();
    let reader = Rc::clone(&clock);
    pump(
        &mut source,
        Path::new(ROOT),
        &mut move || reader.get(),
        &mut |events| batches.push((clock.get() - start, events)),
    );
    (batches, source.timeouts)
}

fn changes(raws: Vec<RawChange>) -> Poll {
    Poll::Changes(raws)
}

#[test]
fn the_pump_releases_events_after_the_quiet_period() {
    let (batches, timeouts) = run_pump(vec![
        (
            0,
            changes(vec![
                raw(RawAction::Added, "a"),
                raw(RawAction::Modified, "a"),
            ]),
        ),
        (50, Poll::Timeout),
        (0, Poll::Stop),
    ]);
    assert_eq!(
        batches,
        [(
            Duration::from_millis(50),
            vec![ev("a", FsEventKind::Created)]
        )]
    );
    assert_eq!(timeouts, [None, Some(DEBOUNCE), None]);
}

#[test]
fn separate_bursts_are_delivered_separately() {
    let (batches, _) = run_pump(vec![
        (0, changes(vec![raw(RawAction::Added, "a")])),
        (50, Poll::Timeout),
        (200, changes(vec![raw(RawAction::Removed, "a")])),
        (50, Poll::Timeout),
        (0, Poll::Stop),
    ]);
    let kinds: Vec<_> = batches.iter().map(|(_, events)| events.clone()).collect();
    assert_eq!(
        kinds,
        [
            vec![ev("a", FsEventKind::Created)],
            vec![ev("a", FsEventKind::Removed)]
        ]
    );
}

#[test]
fn stopping_flushes_pending_events_and_an_idle_stop_delivers_nothing() {
    let (batches, _) = run_pump(vec![
        (0, changes(vec![raw(RawAction::Modified, "a")])),
        (10, Poll::Stop),
    ]);
    assert_eq!(
        batches,
        [(
            Duration::from_millis(10),
            vec![ev("a", FsEventKind::Modified)]
        )]
    );

    let (idle, timeouts) = run_pump(vec![(0, Poll::Stop)]);
    assert!(idle.is_empty());
    assert_eq!(timeouts, [None]);
}

#[test]
fn a_lost_watch_ends_with_a_rescan_of_the_root() {
    let (batches, _) = run_pump(vec![
        (0, changes(vec![raw(RawAction::Modified, "a")])),
        (5, Poll::Lost),
    ]);
    let rescan = FsEvent {
        path: PathBuf::from(ROOT),
        kind: FsEventKind::Rescan,
    };
    assert_eq!(
        batches,
        [(
            Duration::from_millis(5),
            vec![ev("a", FsEventKind::Modified), rescan.clone()]
        )]
    );

    let (alone, _) = run_pump(vec![(0, Poll::Lost)]);
    assert_eq!(alone, [(Duration::ZERO, vec![rescan])]);
}

#[test]
fn the_pump_delivers_during_a_steady_stream_at_the_maximum_delay() {
    let mut items: Vec<(u64, Poll)> = (0..14)
        .map(|_| (40, changes(vec![raw(RawAction::Modified, "a")])))
        .collect();
    items.push((100, Poll::Stop));
    let (batches, _) = run_pump(items);
    // The 14th change arrives at 560 ms; the window opened at 40 ms and is capped at 500 ms.
    assert_eq!(
        batches,
        [(
            Duration::from_millis(560),
            vec![ev("a", FsEventKind::Modified)]
        )]
    );
}

fn temp_file(content: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    std::fs::write(&path, content).unwrap();
    (dir, path)
}

fn stamp_of(path: &Path) -> SaveStamp {
    let meta = RealFs.metadata(path).unwrap();
    SaveStamp::from_read(&meta, &std::fs::read(path).unwrap())
}

fn set_modified(path: &Path, seconds: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)),
    )
    .unwrap();
}

#[test]
fn own_writes_are_not_external_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("doc.md");
    let stamp = save_atomic(&RealFs, &ThreadSleeper, &path, b"saved by us").unwrap();
    let mut tracker = ExternalChanges::new(Arc::new(RealFs));
    assert_eq!(tracker.stamp(&path), None);
    tracker.track(&path, stamp);
    assert_eq!(tracker.stamp(&path), Some(stamp));
    assert_eq!(tracker.check(&path), DocChange::Unchanged);

    // Tracking is case-insensitive like the file system.
    let shouting = PathBuf::from(path.to_string_lossy().to_uppercase());
    assert_eq!(tracker.stamp(&shouting), Some(stamp));

    // Tracking again replaces the stamp (after the next own save).
    let newer = save_atomic(&RealFs, &ThreadSleeper, &path, b"saved again, longer").unwrap();
    assert_eq!(
        tracker.check(&path),
        DocChange::Changed,
        "the old stamp no longer fits"
    );
    tracker.track(&path, newer);
    assert_eq!(tracker.check(&path), DocChange::Unchanged);

    tracker.untrack(&path);
    assert_eq!(tracker.stamp(&path), None);
    std::fs::write(&path, "changed behind our back").unwrap();
    assert_eq!(
        tracker.check(&path),
        DocChange::Unchanged,
        "untracked paths are not judged"
    );
}

#[test]
fn external_modifications_deletions_and_touches_are_told_apart() {
    let (_dir, path) = temp_file("abc");
    let mut tracker = ExternalChanges::new(Arc::new(RealFs));
    tracker.track(&path, stamp_of(&path));

    // Only the timestamp moved: same size, same bytes.
    set_modified(&path, 1_000_000_000);
    assert_eq!(tracker.check(&path), DocChange::Unchanged);

    // Same size, different bytes.
    std::fs::write(&path, "abd").unwrap();
    set_modified(&path, 1_100_000_000);
    assert_eq!(tracker.check(&path), DocChange::Changed);

    // Different size.
    std::fs::write(&path, "abcdef").unwrap();
    assert_eq!(tracker.check(&path), DocChange::Changed);

    std::fs::remove_file(&path).unwrap();
    assert_eq!(tracker.check(&path), DocChange::Deleted);
}

#[test]
fn unverifiable_files_count_as_changed() {
    let (_dir, path) = temp_file("abc");
    let fault = Arc::new(FaultFs::new());
    let mut tracker = ExternalChanges::new(fault.clone());
    tracker.track(&path, stamp_of(&path));

    fault.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    assert_eq!(tracker.check(&path), DocChange::Changed);
    assert_eq!(tracker.check(&path), DocChange::Unchanged);

    // Same size, newer timestamp, but the content cannot be read to compare.
    set_modified(&path, 1_000_000_000);
    fault.fail(FsOp::Read, FsFailure::SharingViolation, 1);
    assert_eq!(tracker.check(&path), DocChange::Changed);
    assert_eq!(tracker.check(&path), DocChange::Unchanged);
}

#[test]
fn events_are_classified_per_document() {
    let dir = tempfile::tempdir().unwrap();
    let file = |name: &str, content: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    };
    let own = file("own.md", "own");
    let edited = file("edited.md", "edited");
    let deleted = file("deleted.md", "deleted");
    let moved = file("moved.md", "moved");
    let bystander = file("bystander.md", "bystander");
    let mut tracker = ExternalChanges::new(Arc::new(RealFs));
    for path in [&own, &edited, &deleted, &moved] {
        tracker.track(path, stamp_of(path));
    }
    std::fs::write(&edited, "edited by someone").unwrap();
    std::fs::remove_file(&deleted).unwrap();
    let moved_to = dir.path().join("moved-away.md");
    std::fs::rename(&moved, &moved_to).unwrap();

    let event = |path: &Path, kind| FsEvent {
        path: path.to_path_buf(),
        kind,
    };
    let events = [
        event(&own, FsEventKind::Modified),
        event(&edited, FsEventKind::Modified),
        event(&bystander, FsEventKind::Modified),
        event(&deleted, FsEventKind::Removed),
        event(
            &moved_to,
            FsEventKind::Renamed {
                from: moved.clone(),
            },
        ),
    ];
    assert_eq!(
        tracker.classify(&events),
        [
            (edited.clone(), DocChange::Changed),
            (deleted.clone(), DocChange::Deleted),
            (
                moved.clone(),
                DocChange::Renamed {
                    to: moved_to.clone()
                }
            ),
        ]
    );
    assert!(tracker.classify(&[]).is_empty());

    // The last decision about a document wins and is reported once.
    let twice = [
        event(&edited, FsEventKind::Modified),
        event(
            &moved_to,
            FsEventKind::Renamed {
                from: edited.clone(),
            },
        ),
    ];
    assert_eq!(
        tracker.classify(&twice),
        [(
            edited.clone(),
            DocChange::Renamed {
                to: moved_to.clone()
            }
        )]
    );

    // A rename between untracked names tells us nothing; a shouted path still finds its document.
    let untracked = [event(
        &bystander,
        FsEventKind::Renamed {
            from: dir.path().join("x.md"),
        },
    )];
    assert!(tracker.classify(&untracked).is_empty());
    let shouting = PathBuf::from(edited.to_string_lossy().to_uppercase());
    assert_eq!(
        tracker.classify(&[event(&shouting, FsEventKind::Modified)]),
        [(shouting.clone(), DocChange::Changed)]
    );
}

#[test]
fn a_rescan_rechecks_every_tracked_document() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for name in ["a.md", "b.md", "c.md", "d.md"] {
        let path = dir.path().join(name);
        std::fs::write(&path, name).unwrap();
        paths.push(path);
    }
    let mut tracker = ExternalChanges::new(Arc::new(RealFs));
    for path in &paths {
        tracker.track(path, stamp_of(path));
    }
    std::fs::write(&paths[1], "b was rewritten").unwrap();
    std::fs::remove_file(&paths[2]).unwrap();

    let rescan = FsEvent {
        path: dir.path().to_path_buf(),
        kind: FsEventKind::Rescan,
    };
    let mut found = tracker.classify(&[rescan]);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        found,
        [
            (paths[1].clone(), DocChange::Changed),
            (paths[2].clone(), DocChange::Deleted)
        ]
    );
}

// ---- the real ReadDirectoryChangesW watcher -------------------------------------------------

const WAIT: Duration = Duration::from_secs(15);

fn same(a: &Path, b: &Path) -> bool {
    path_key(a) == path_key(b)
}

/// Waits (bounded) for the first event satisfying `wanted`; other events are skipped.
fn wait_for(
    receiver: &Receiver<FsEvent>,
    what: &str,
    mut wanted: impl FnMut(&FsEvent) -> bool,
) -> FsEvent {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(left) {
            Ok(event) if wanted(&event) => return event,
            Ok(_) => {}
            Err(error) => panic!("no event for {what}: {error:?}"),
        }
    }
}

/// Collects events until `quiet` passes without a new one.
fn drain_until_quiet(receiver: &Receiver<FsEvent>, quiet: Duration) -> Vec<FsEvent> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.recv_timeout(quiet) {
        events.push(event);
    }
    events
}

#[test]
fn the_watcher_reports_create_modify_rename_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let woken = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&woken);
    let (watcher, receiver) = Watcher::start(dir.path(), move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    assert_eq!(watcher.root(), dir.path());
    let a = dir.path().join("a.md");
    let b = dir.path().join("b.md");

    std::fs::write(&a, "one").unwrap();
    let created = wait_for(&receiver, "create", |e| same(&e.path, &a));
    assert_eq!(created.kind, FsEventKind::Created);
    assert!(woken.load(Ordering::SeqCst) >= 1, "the wake callback ran");

    std::fs::write(&a, "one and more").unwrap();
    let modified = wait_for(&receiver, "modify", |e| {
        same(&e.path, &a) && e.kind == FsEventKind::Modified
    });
    assert_eq!(modified.path, a);

    std::fs::rename(&a, &b).unwrap();
    let renamed = wait_for(&receiver, "rename", |e| same(&e.path, &b));
    assert_eq!(renamed.kind, FsEventKind::Renamed { from: a.clone() });

    std::fs::remove_file(&b).unwrap();
    wait_for(&receiver, "delete", |e| {
        same(&e.path, &b) && e.kind == FsEventKind::Removed
    });
}

#[test]
fn the_watcher_sees_the_whole_tree() {
    let dir = tempfile::tempdir().unwrap();
    let (_watcher, receiver) = Watcher::start(dir.path(), || {}).unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let created = wait_for(&receiver, "folder", |e| same(&e.path, &sub));
    assert_eq!(created.kind, FsEventKind::Created);

    let inner = sub.join("inner.md");
    std::fs::write(&inner, "x").unwrap();
    wait_for(&receiver, "inner file", |e| {
        same(&e.path, &inner) && e.kind == FsEventKind::Created
    });

    let moved = dir.path().join("moved");
    std::fs::rename(&sub, &moved).unwrap();
    let renamed = wait_for(&receiver, "folder rename", |e| same(&e.path, &moved));
    assert_eq!(renamed.kind, FsEventKind::Renamed { from: sub.clone() });
}

#[test]
fn bursts_are_coalesced_and_short_lived_files_vanish() {
    let dir = tempfile::tempdir().unwrap();
    let (_watcher, receiver) = Watcher::start(dir.path(), || {}).unwrap();

    // Many quick writes to one new file: one event.
    let busy = dir.path().join("busy.md");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&busy).unwrap();
        for line in 0..20 {
            writeln!(file, "line {line}").unwrap();
        }
    }
    // A file that is created and deleted inside one window: no event at all.
    let blink = dir.path().join("blink.md");
    std::fs::write(&blink, "x").unwrap();
    std::fs::remove_file(&blink).unwrap();

    let events = drain_until_quiet(&receiver, Duration::from_millis(600));
    let busy_events: Vec<_> = events.iter().filter(|e| same(&e.path, &busy)).collect();
    assert_eq!(busy_events[0].kind, FsEventKind::Created, "{events:?}");
    let creations = busy_events
        .iter()
        .filter(|e| e.kind == FsEventKind::Created)
        .count();
    assert_eq!(creations, 1, "{events:?}");
    assert!(events.iter().all(|e| !same(&e.path, &blink)), "{events:?}");
}

#[test]
fn own_saves_are_filtered_and_foreign_edits_are_flagged_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let doc = dir.path().join("doc.md");
    let first = save_atomic(&RealFs, &ThreadSleeper, &doc, b"first version").unwrap();
    let mut tracker = ExternalChanges::new(Arc::new(RealFs));
    tracker.track(&doc, first);
    let (_watcher, receiver) = Watcher::start(dir.path(), || {}).unwrap();

    let second = save_atomic(
        &RealFs,
        &ThreadSleeper,
        &doc,
        b"second, written by the editor",
    )
    .unwrap();
    tracker.track(&doc, second);
    let own = drain_until_quiet(&receiver, Duration::from_millis(700));
    assert!(!own.is_empty(), "the save was observed");
    assert!(tracker.classify(&own).is_empty(), "{own:?}");

    std::fs::write(&doc, "edited by another program entirely").unwrap();
    let foreign = drain_until_quiet(&receiver, Duration::from_millis(700));
    assert_eq!(
        tracker.classify(&foreign),
        [(doc.clone(), DocChange::Changed)],
        "{foreign:?}"
    );
}

#[test]
fn dropping_the_watcher_stops_its_thread_and_closes_the_channel() {
    let dir = tempfile::tempdir().unwrap();
    let (watcher, receiver) = Watcher::start(dir.path(), || {}).unwrap();
    let started = Instant::now();
    drop(watcher);
    assert!(started.elapsed() < Duration::from_secs(10));
    loop {
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error, RecvTimeoutError::Disconnected);
                break;
            }
        }
    }
    // The folder can be removed while no watcher holds it.
    std::fs::remove_dir(dir.path()).unwrap();
    std::fs::create_dir(dir.path()).unwrap();
}

#[test]
fn watching_a_missing_folder_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let error = Watcher::start(&missing, || {}).unwrap_err();
    assert_eq!(error, FsError::NotFound(missing));
}

#[test]
fn a_watch_that_cannot_be_read_ends_with_a_rescan() {
    let (_dir, file) = temp_file("not a folder");
    let (_watcher, receiver) = Watcher::start(&file, || {}).unwrap();
    let rescan = wait_for(&receiver, "rescan", |e| e.kind == FsEventKind::Rescan);
    assert_eq!(rescan.path, file);
    match receiver.recv_timeout(WAIT) {
        Err(RecvTimeoutError::Disconnected) => {}
        other => panic!("the channel should close after the rescan, got {other:?}"),
    }
}
